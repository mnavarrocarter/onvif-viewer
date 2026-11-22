//! Browser PTZ leases. A camera has at most one browser owner at a time.
use crate::storage::Source;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const LEASE: Duration = Duration::from_millis(1500);

struct Movement {
    owner: String,
    token: String,
    camera: onvif::Camera,
    lease: crate::ptz_lease::Lease,
    renewed: Instant,
}

async fn stop(movement: Movement) -> Result<(), String> {
    match movement.camera.ptz_stop(&movement.token).await {
        Ok(()) => {
            movement.lease.stopped();
            Ok(())
        }
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Default)]
pub struct PtzLeases {
    cameras: Mutex<HashMap<u64, Arc<Mutex<Option<Movement>>>>>,
}

impl PtzLeases {
    pub async fn command(
        &self,
        source: &Source,
        session: &str,
        token: &str,
        x: f32,
        y: f32,
    ) -> Result<(), String> {
        if !x.is_finite()
            || !y.is_finite()
            || !(-1.0..=1.0).contains(&x)
            || !(-1.0..=1.0).contains(&y)
        {
            return Err("Invalid PTZ direction".into());
        }
        let slot = {
            let mut cameras = self.cameras.lock().await;
            cameras.entry(source.id).or_default().clone()
        };
        let mut active = slot.lock().await;
        if let Some(current) = active.as_ref() {
            if current.owner != session && current.renewed.elapsed() < LEASE {
                return Err("Another viewer controls this camera".into());
            }
        }
        if active
            .as_ref()
            .is_some_and(|m| m.owner != session || m.token != token || m.renewed.elapsed() >= LEASE)
        {
            let previous = active.take().unwrap();
            stop(previous).await?;
        }
        if x == 0.0 && y == 0.0 {
            if let Some(previous) = active.take() {
                stop(previous).await?;
            }
            return Ok(());
        }
        if active.is_none() {
            let camera = onvif::connect(&source.address, &source.username, &source.password)
                .await
                .map_err(|e| e.to_string())?;
            if !camera
                .ptz_supported(token)
                .await
                .map_err(|e| e.to_string())?
            {
                return Err("This profile does not support continuous PTZ".into());
            }
            let lease = crate::ptz_lease::Lease::acquire(source.id)
                .ok_or("Another viewer controls this camera")?;
            *active = Some(Movement {
                owner: session.to_owned(),
                token: token.to_owned(),
                camera,
                lease,
                renewed: Instant::now(),
            });
        }
        let movement = active.as_mut().unwrap();
        if let Err(error) = movement.camera.ptz_move(token, x, y).await {
            let message = error.to_string();
            let movement = active.take().unwrap();
            let _ = stop(movement).await;
            return Err(message);
        }
        movement.renewed = Instant::now();
        let slot = slot.clone();
        tokio::spawn(async move {
            tokio::time::sleep(LEASE).await;
            let mut active = slot.lock().await;
            if active
                .as_ref()
                .is_some_and(|m| m.renewed.elapsed() >= LEASE)
            {
                let movement = active.take().unwrap();
                let _ = stop(movement).await;
            }
        });
        Ok(())
    }

    pub async fn stop_session(&self, session: &str) {
        let slots: Vec<_> = self.cameras.lock().await.values().cloned().collect();
        for slot in slots {
            let mut active = slot.lock().await;
            if active.as_ref().is_some_and(|m| m.owner == session) {
                let movement = active.take().unwrap();
                let _ = stop(movement).await;
            }
        }
    }
}
