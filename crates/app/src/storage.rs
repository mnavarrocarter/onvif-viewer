#[derive(Clone)]
pub struct Source {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub username: String,
    pub password: String,
    pub model: String,
    pub serial: String,
    pub profiles: Vec<onvif::Profile>,
    pub selected_profile: Option<String>,
}

impl Source {
    pub fn profile_label(profile: &onvif::Profile) -> String {
        let mut label = profile.name.clone();
        if let (Some(width), Some(height)) = (profile.width, profile.height) {
            label.push_str(&format!("  {width}×{height}"));
        }
        if let Some(codec) = &profile.codec {
            label.push_str(&format!("  {codec}"));
        }
        if let Some(codec) = &profile.audio_codec {
            label.push_str(&format!("  Audio: {codec}"));
        }
        label
    }

    pub fn default_profile(profiles: &[onvif::Profile]) -> Option<String> {
        profiles
            .iter()
            .find(|profile| {
                profile.codec.is_some() || profile.width.is_some() || profile.height.is_some()
            })
            .or_else(|| profiles.first())
            .map(|profile| profile.token.clone())
    }

    pub fn selected_profile_label(&self) -> String {
        self.selected_profile
            .as_ref()
            .and_then(|token| self.profiles.iter().find(|profile| &profile.token == token))
            .map(Self::profile_label)
            .unwrap_or_else(|| "No profile available".to_owned())
    }
}

pub type StorageError = Box<dyn std::error::Error + Send + Sync>;

pub trait SourceStore {
    fn list(&self) -> Result<Vec<Source>, StorageError>;
    fn insert(&mut self, source: Source) -> Result<(), StorageError>;
    fn update(&mut self, source: Source) -> Result<(), StorageError>;
    fn remove(&mut self, id: u64) -> Result<(), StorageError>;
}

#[derive(Default)]
pub struct MemoryStore {
    sources: Vec<Source>,
}

impl SourceStore for MemoryStore {
    fn list(&self) -> Result<Vec<Source>, StorageError> {
        Ok(self.sources.clone())
    }

    fn insert(&mut self, source: Source) -> Result<(), StorageError> {
        self.sources.push(source);
        Ok(())
    }
    fn update(&mut self, source: Source) -> Result<(), StorageError> {
        let stored = self
            .sources
            .iter_mut()
            .find(|item| item.id == source.id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "Camera not found"))?;
        *stored = source;
        Ok(())
    }

    fn remove(&mut self, id: u64) -> Result<(), StorageError> {
        self.sources.retain(|source| source.id != id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryStore, Source, SourceStore};

    fn source(id: u64, username: &str, password: &str) -> Source {
        Source {
            id,
            name: format!("Camera {id}"),
            address: format!("http://camera-{id}/onvif/device_service"),
            username: username.into(),
            password: password.into(),
            model: "Model".into(),
            serial: "Serial".into(),
            profiles: Vec::new(),
            selected_profile: None,
        }
    }

    #[test]
    fn adds_lists_and_removes_sources() {
        let mut store = MemoryStore::default();
        assert!(store.list().unwrap().is_empty());

        store
            .insert(source(1, "first-user", "first-password"))
            .unwrap();
        store
            .insert(source(2, "second-user", "second-password"))
            .unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].id, 1);
        assert_eq!(sources[0].name, "Camera 1");
        assert_eq!(sources[0].address, "http://camera-1/onvif/device_service");
        assert_eq!(sources[1].id, 2);

        store.remove(1).unwrap();
        store.remove(99).unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, 2);
        store.remove(2).unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn updates_the_selected_profile_without_replacing_other_sources() {
        let mut store = MemoryStore::default();
        store.insert(source(1, "user", "password")).unwrap();
        store.insert(source(2, "other", "other-password")).unwrap();
        let mut updated = store.list().unwrap().remove(0);
        updated.selected_profile = Some("profile-2".into());
        updated.name = "Updated".into();
        store.update(updated).unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources[0].selected_profile.as_deref(), Some("profile-2"));
        assert_eq!(sources[0].name, "Updated");
        assert_eq!(sources[1].name, "Camera 2");
        assert!(store.update(source(99, "", "")).is_err());
    }

    #[test]
    fn credentials_are_isolated_per_source_and_from_list_copies() {
        let mut store = MemoryStore::default();
        store
            .insert(source(1, "first-user", "first-password"))
            .unwrap();
        store
            .insert(source(2, "second-user", "second-password"))
            .unwrap();

        let mut sources = store.list().unwrap();
        assert_eq!(sources[0].username, "first-user");
        assert_eq!(sources[0].password, "first-password");
        assert_eq!(sources[1].username, "second-user");
        assert_eq!(sources[1].password, "second-password");
        sources[0].username.clear();
        sources[0].password.clear();

        let sources = store.list().unwrap();
        assert_eq!(sources[0].username, "first-user");
        assert_eq!(sources[0].password, "first-password");
        assert_eq!(sources[1].username, "second-user");
        assert_eq!(sources[1].password, "second-password");
    }
}
