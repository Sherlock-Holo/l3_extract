use std::sync::Arc;

use crossbeam_channel::{Receiver, SendError, Sender, TryRecvError};
use event_listener::{listener, Event};

#[derive(Debug)]
pub struct NotifySender<T> {
    sender: Sender<T>,
    event: Arc<Event>,
}

impl<T> Clone for NotifySender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            event: self.event.clone(),
        }
    }
}

impl<T> NotifySender<T> {
    pub fn send(&self, v: T) -> Result<(), SendError<T>> {
        self.sender.send(v)?;
        self.event.notify(1);

        Ok(())
    }
}

#[derive(Debug)]
pub struct NotifyReceiver<T> {
    receiver: Receiver<T>,
    event: Arc<Event>,
}

impl<T> NotifyReceiver<T> {
    pub async fn wait(&mut self) {
        loop {
            if !self.receiver.is_empty() {
                return;
            }

            listener!(self.event => listener);

            if !self.receiver.is_empty() {
                return;
            }

            listener.await;
        }
    }

    pub fn collect_nonblock(&mut self) -> Result<Vec<T>, TryRecvError> {
        let mut items = Vec::with_capacity(self.receiver.len());
        loop {
            match self.receiver.try_recv() {
                Ok(item) => {
                    items.push(item);
                }
                Err(TryRecvError::Disconnected) => return Err(TryRecvError::Disconnected),
                Err(TryRecvError::Empty) => break,
            }
        }

        Ok(items)
    }
}

pub fn channel<T>() -> (NotifySender<T>, NotifyReceiver<T>) {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let event = Arc::new(Event::new());

    (
        NotifySender {
            sender,
            event: event.clone(),
        },
        NotifyReceiver { receiver, event },
    )
}
