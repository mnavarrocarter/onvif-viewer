//! Process-wide PTZ ownership, shared by native and browser controls.
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Entry {
    owner: Uuid,
    release_after: Option<Instant>,
}
static ENTRIES: LazyLock<Mutex<HashMap<u64, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub struct Lease {
    camera: u64,
    owner: Uuid,
}
impl Lease {
    pub fn acquire(camera: u64) -> Option<Self> {
        let mut entries = ENTRIES.lock().unwrap();
        if entries.get(&camera).is_some_and(|entry| {
            entry
                .release_after
                .is_none_or(|deadline| deadline > Instant::now())
        }) {
            return None;
        }
        let owner = Uuid::new_v4();
        entries.insert(
            camera,
            Entry {
                owner,
                release_after: None,
            },
        );
        Some(Self { camera, owner })
    }
    /// Release immediately only after the camera acknowledges Stop.
    pub fn stopped(self) {
        let mut entries = ENTRIES.lock().unwrap();
        if entries
            .get(&self.camera)
            .is_some_and(|entry| entry.owner == self.owner)
        {
            entries.remove(&self.camera);
        }
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut entries = ENTRIES.lock().unwrap();
        if let Some(entry) = entries
            .get_mut(&self.camera)
            .filter(|entry| entry.owner == self.owner)
        {
            // ONVIF requests at most five seconds of motion; include a margin.
            entry.release_after = Some(Instant::now() + Duration::from_secs(6));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn competing_owners_wait_for_stop_or_timeout() {
        let camera = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap());
        let first = Lease::acquire(camera).unwrap();
        assert!(Lease::acquire(camera).is_none());
        first.stopped();
        let second = Lease::acquire(camera).unwrap();
        drop(second);
        assert!(Lease::acquire(camera).is_none());
    }
}
