// Copyright (c) Sean Lawlor
//
// This source code is licensed under both the MIT license found in the
// LICENSE-MIT file in the root directory of this source tree.

//! A reference counted actor which can be passed around as needed

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::Duration;

use super::errors::MessagingErr;
use super::messages::{BoxedMessage, Signal};
use super::supervision::SupervisionTree;
use super::SupervisionEvent;
use crate::port::{
    BoundedInputPort, BoundedInputPortReceiver, InputPort, InputPortReceiver, RpcReplyPort,
};
use crate::rpc::{self, CallResult};
use crate::{ActorHandler, ActorId, Message};

/// [ActorStatus] represents the status of an actor
#[derive(Debug, Clone, Eq, PartialEq, Copy)]
#[repr(u8)]
pub enum ActorStatus {
    /// Created, but not yet started
    Unstarted = 0u8,
    /// Starting
    Starting = 1u8,
    /// Executing (or waiting on messages)
    Running = 2u8,
    /// Upgrading
    Upgrading = 3u8,
    /// Stopping
    Stopping = 4u8,
    /// Dead
    Stopped = 5u8,
}

/// Actor states where operations can continue to interact with an agent
pub const ACTIVE_STATES: [ActorStatus; 3] = [
    ActorStatus::Starting,
    ActorStatus::Running,
    ActorStatus::Upgrading,
];

/// The collection of ports an actor needs to listen to
pub(crate) struct ActorPortSet {
    pub(crate) signal_rx: BoundedInputPortReceiver<Signal>,
    pub(crate) supervisor_rx: InputPortReceiver<SupervisionEvent>,
    pub(crate) message_rx: InputPortReceiver<BoxedMessage>,
}

/// The inner-properties of an Actor
struct ActorProperties {
    id: ActorId,
    name: Option<String>,
    status: Arc<AtomicU8>,
    signal: BoundedInputPort<Signal>,
    supervision: InputPort<SupervisionEvent>,
    message: InputPort<BoxedMessage>,
    tree: SupervisionTree,
}
impl ActorProperties {
    pub fn new(
        name: Option<String>,
    ) -> (
        Self,
        BoundedInputPortReceiver<Signal>,
        InputPortReceiver<SupervisionEvent>,
        InputPortReceiver<BoxedMessage>,
    ) {
        let (tx, rx) = mpsc::channel(2);
        let (tx2, rx2) = mpsc::unbounded_channel();
        let (tx3, rx3) = mpsc::unbounded_channel();
        (
            Self {
                id: crate::ACTOR_ID_ALLOCATOR.fetch_add(1u64, Ordering::Relaxed),
                name,
                status: Arc::new(AtomicU8::new(ActorStatus::Unstarted as u8)),
                signal: tx,
                supervision: tx2,
                message: tx3,
                tree: SupervisionTree::default(),
            },
            rx,
            rx2,
            rx3,
        )
    }

    pub fn get_status(&self) -> ActorStatus {
        match self.status.load(Ordering::Relaxed) {
            0u8 => ActorStatus::Unstarted,
            1u8 => ActorStatus::Starting,
            2u8 => ActorStatus::Running,
            3u8 => ActorStatus::Upgrading,
            4u8 => ActorStatus::Stopping,
            _ => ActorStatus::Stopped,
        }
    }

    pub fn set_status(&self, status: ActorStatus) {
        self.status.store(status as u8, Ordering::Relaxed);
    }

    pub fn send_signal(&self, signal: Signal) -> Result<(), MessagingErr> {
        self.signal.try_send(signal).map_err(|e| e.into())
    }

    pub fn send_supervisor_evt(&self, message: SupervisionEvent) -> Result<(), MessagingErr> {
        self.supervision.send(message).map_err(|e| e.into())
    }

    pub fn send_message(&self, message: BoxedMessage) -> Result<(), MessagingErr> {
        self.message.send(message).map_err(|e| e.into())
    }
}

/// A handy-dandy reference to and actor and their inner properties
/// which can be cloned and passed around
#[derive(Clone)]
pub struct ActorCell {
    inner: Arc<ActorProperties>,
}

impl std::fmt::Debug for ActorCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(name) = self.get_name() {
            write!(f, "Actor '{}' (id: {})", name, self.get_id())
        } else {
            write!(f, "Actor with id: {}", self.get_id())
        }
    }
}

impl ActorCell {
    /// Construct a new [ActorCell] pointing to an [super::Actor] and return the message reception channels as a [ActorPortSet]
    ///
    /// * `name` - Optional name for the actor
    ///
    /// Returns a tuple [(ActorCell, ActorPortSet)] to bootstrap the [Actor]
    pub(crate) fn new(name: Option<String>) -> (Self, ActorPortSet) {
        let (props, rx1, rx2, rx3) = ActorProperties::new(name);
        (
            Self {
                inner: Arc::new(props),
            },
            ActorPortSet {
                signal_rx: rx1,
                supervisor_rx: rx2,
                message_rx: rx3,
            },
        )
    }

    /// Retrieve the [super::Actor]'s unique identifier [ActorId]
    pub fn get_id(&self) -> ActorId {
        self.inner.id
    }

    /// Retrieve the [super::Actor]'s name
    pub fn get_name(&self) -> Option<String> {
        self.inner.name.clone()
    }

    /// Retrieve the current status of an [super::Actor]
    ///
    /// Returns the [super::Actor]'s current [ActorStatus]
    pub fn get_status(&self) -> ActorStatus {
        self.inner.get_status()
    }

    /// Set the status of the [super::Actor]
    ///
    /// * `status` - The [ActorStatus] to set
    pub(crate) fn set_status(&self, status: ActorStatus) {
        self.inner.set_status(status)
    }

    /// Terminate this [super::Actor] and all it's children
    pub(crate) fn terminate(&self) {
        // we don't need to nofity of exit if we're already stopping or stopped
        if self.get_status() as u8 <= ActorStatus::Upgrading as u8 {
            // kill myself immediately. Ignores failures, as a failure means either
            // 1. we're already dead or
            // 2. the channel is full of "signals"
            self.stop();
        }

        // notify children they should die. They will unlink themselves from the supervisor
        self.inner.tree.terminate_children();
    }

    /// Link this [super::Actor] to the supervisor
    ///
    /// * `supervisor` - The supervisor to link this [super::Actor] to
    pub fn link(&self, supervisor: ActorCell) {
        supervisor.inner.tree.insert_parent(self.clone());
        self.inner.tree.insert_child(supervisor);
    }

    /// Unlink this [super::Actor] from the supervisor
    ///
    /// * `supervisor` - The supervisor to unlink this [super::Actor] from
    pub fn unlink(&self, supervisor: ActorCell) {
        supervisor.inner.tree.remove_parent(self.clone());
        self.inner.tree.remove_child(supervisor);
    }

    /// Stop this [super::Actor], by sending [Signal::Exit]
    pub fn stop(&self) {
        // ignore failures, since either the actor is already dead
        // or the channel is full of "signals" which is also fine
        // since it'll die shortly
        let _ = self.inner.send_signal(Signal::Exit);
    }

    /// Send a supervisor event to the supervisory port
    ///
    /// * `message` - The [SupervisionEvent] to send to the supervisory port
    ///
    /// Returns [Ok(())] on successful message send, [Err(MessagingErr)] otherwise
    pub(crate) fn send_supervisor_evt(
        &self,
        message: SupervisionEvent,
    ) -> Result<(), MessagingErr> {
        self.inner.send_supervisor_evt(message)
    }

    /// Send a strongly-typed message, constructing the boxed message on the fly
    ///
    /// Note: The type requirement of `TActor` assures that `TMsg` is the supported
    /// message type for `TActor` such that we can't send boxed messages of an unsupported
    /// type to the specified actor.
    ///
    /// * `message` - The message to send
    ///
    /// Returns [Ok(())] on successful message send, [Err(MessagingErr)] otherwise
    pub fn send_message<TActor, TMsg>(&self, message: TMsg) -> Result<(), MessagingErr>
    where
        TActor: ActorHandler<Msg = TMsg>,
        TMsg: Message,
    {
        self.inner.send_message(BoxedMessage::new(message))
    }

    /// Notify the supervisors that a supervision event occurred
    ///
    /// * `evt` - The event to send to this [super::Actor]'s supervisors
    pub fn notify_supervisors<TActor, TState>(&self, evt: SupervisionEvent)
    where
        TActor: ActorHandler<State = TState>,
        TState: crate::State,
    {
        self.inner.tree.notify_supervisors::<TActor, _>(evt)
    }

    /// Alias of [rpc::cast]
    pub fn cast<TActor, TMsg>(&self, msg: TMsg) -> Result<(), MessagingErr>
    where
        TActor: ActorHandler<Msg = TMsg>,
        TMsg: Message,
    {
        rpc::cast::<TActor, _>(self, msg)
    }

    /// Alias of [rpc::call]
    pub async fn call<TActor, TMsg, TReply, TMsgBuilder>(
        &self,
        msg_builder: TMsgBuilder,
        timeout_option: Option<Duration>,
    ) -> Result<CallResult<TReply>, MessagingErr>
    where
        TActor: ActorHandler<Msg = TMsg>,
        TMsg: Message,
        TMsgBuilder: FnOnce(RpcReplyPort<TReply>) -> TMsg,
    {
        rpc::call::<TActor, TMsg, TReply, TMsgBuilder>(self, msg_builder, timeout_option).await
    }

    /// Alias of [rpc::call_and_forward]
    pub fn call_and_forward<
        TActor,
        TForwardActor,
        TMsg,
        TReply,
        TMsgBuilder,
        FwdMapFn,
        TForwardMessage,
    >(
        &self,
        msg_builder: TMsgBuilder,
        response_forward: ActorCell,
        forward_mapping: FwdMapFn,
        timeout_option: Option<Duration>,
    ) -> Result<tokio::task::JoinHandle<CallResult<Result<(), MessagingErr>>>, MessagingErr>
    where
        TActor: ActorHandler<Msg = TMsg>,
        TMsg: Message,
        TReply: Message,
        TMsgBuilder: FnOnce(RpcReplyPort<TReply>) -> TMsg,
        TForwardActor: ActorHandler<Msg = TForwardMessage>,
        FwdMapFn: FnOnce(TReply) -> TForwardMessage + Send + 'static,
        TForwardMessage: Message,
    {
        rpc::call_and_forward::<
            TActor,
            TForwardActor,
            TMsg,
            TReply,
            TMsgBuilder,
            FwdMapFn,
            TForwardMessage,
        >(
            self,
            msg_builder,
            response_forward,
            forward_mapping,
            timeout_option,
        )
    }

    /// Test utility to retrieve a clone of the underlying supervision tree
    #[cfg(test)]
    pub(crate) fn get_tree(&self) -> SupervisionTree {
        self.inner.tree.clone()
    }
}
