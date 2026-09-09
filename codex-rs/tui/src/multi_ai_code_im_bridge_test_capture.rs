//! Per-test capture at the reliable transport boundary. No global endpoint or
//! environment mutation; production notification and payload construction run.

use serde_json::Value;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

thread_local! {
    static MESSAGES: RefCell<Option<Vec<(Value, String)>>> = const { RefCell::new(None) };
}

pub(crate) struct Capture(PhantomData<Rc<()>>);

impl Capture {
    pub(crate) fn start() -> Self {
        MESSAGES.with(|messages| {
            assert!(messages.borrow().is_none(), "capture must not be nested");
            *messages.borrow_mut() = Some(Vec::new());
        });
        Self(PhantomData)
    }

    pub(crate) fn drain(&self) -> Vec<(Value, String)> {
        MESSAGES.with(|messages| std::mem::take(messages.borrow_mut().as_mut().unwrap()))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        MESSAGES.with(|messages| *messages.borrow_mut() = None);
    }
}

pub(super) fn record(payload: &Value, message_id: &str) -> bool {
    MESSAGES.with(|messages| {
        if let Some(messages) = messages.borrow_mut().as_mut() {
            messages.push((payload.clone(), message_id.to_owned()));
            true
        } else {
            false
        }
    })
}
