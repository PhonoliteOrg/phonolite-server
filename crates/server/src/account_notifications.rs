//! Bounded live account invalidations. No durable data or token is stored here.
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::watch;

#[derive(Clone, Default)]
pub(crate) struct AccountNotifications {
    senders: Arc<Mutex<HashMap<String, Weak<watch::Sender<()>>>>>,
}
pub(crate) struct Subscription {
    receiver: watch::Receiver<()>,
    _sender: Arc<watch::Sender<()>>,
}
impl Deref for Subscription {
    type Target = watch::Receiver<()>;
    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}
impl DerefMut for Subscription {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.receiver
    }
}
impl AccountNotifications {
    pub(crate) fn subscribe(&self, user: &str) -> Subscription {
        let mut senders = self.senders.lock().unwrap_or_else(|e| e.into_inner());
        senders.retain(|_, sender| sender.strong_count() != 0);
        let sender = senders
            .get(user)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let sender = Arc::new(watch::channel(()).0);
                senders.insert(user.to_owned(), Arc::downgrade(&sender));
                sender
            });
        Subscription {
            receiver: sender.subscribe(),
            _sender: sender,
        }
    }
    pub(crate) fn notify(&self, user: &str) {
        let sender = self
            .senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(user)
            .and_then(Weak::upgrade);
        if let Some(sender) = sender {
            sender.send_replace(());
        }
    }
    pub(crate) fn notify_all(&self) {
        let senders: Vec<_> = self
            .senders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for sender in senders {
            sender.send_replace(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wakes_every_live_owner_subscription_and_releases_disconnected_accounts() {
        let notifications = AccountNotifications::default();
        let mut first = notifications.subscribe("alice");
        let second = notifications.subscribe("alice");
        let other = notifications.subscribe("bob");
        notifications.notify("alice");
        assert!(first.has_changed().unwrap());
        assert!(second.has_changed().unwrap());
        assert!(!other.has_changed().unwrap());
        first.borrow_and_update();
        notifications.notify_all();
        assert!(first.has_changed().unwrap());
        assert!(other.has_changed().unwrap());
        drop(first);
        drop(second);
        drop(other);
        let _new = notifications.subscribe("new");
        assert_eq!(notifications.senders.lock().unwrap().len(), 1);
    }
}
