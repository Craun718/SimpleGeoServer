use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::data_source::{DataSource, DataSourceInfo};

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum RegistryEvent {
    Mounted(String),
    Unmounted(String),
}

type EventCallback = Arc<dyn Fn(RegistryEvent) + Send + Sync>;

pub struct DataSourceRegistry {
    sources: RwLock<HashMap<String, Arc<dyn DataSource>>>,
    subscribers: RwLock<Vec<EventCallback>>,
}

impl DataSourceRegistry {
    pub fn new() -> Self {
        Self {
            sources: RwLock::new(HashMap::new()),
            subscribers: RwLock::new(Vec::new()),
        }
    }

    pub fn mount(&self, name: String, source: Arc<dyn DataSource>) -> Result<(), String> {
        {
            let mut sources = self.sources.write().map_err(|e| format!("Lock error: {}", e))?;
            if sources.contains_key(&name) {
                return Err(format!("DataSource '{}' already exists", name));
            }
            sources.insert(name.clone(), source);
        }
        self.notify(RegistryEvent::Mounted(name));
        Ok(())
    }

    #[allow(dead_code)]
    pub fn mount_or_replace(&self, name: String, source: Arc<dyn DataSource>) {
        {
            let mut sources = self.sources.write().unwrap();
            sources.insert(name.clone(), source);
        }
        self.notify(RegistryEvent::Mounted(name));
    }

    pub fn unmount(&self, name: &str) -> Result<(), String> {
        {
            let mut sources = self.sources.write().map_err(|e| format!("Lock error: {}", e))?;
            sources.remove(name).ok_or_else(|| format!("DataSource '{}' not found", name))?;
        }
        self.notify(RegistryEvent::Unmounted(name.to_string()));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn DataSource>> {
        let sources = self.sources.read().ok()?;
        sources.get(name).cloned()
    }

    pub fn list(&self) -> Vec<DataSourceInfo> {
        let mut infos: Vec<DataSourceInfo> = self
            .sources
            .read()
            .unwrap()
            .values()
            .map(|s| s.info())
            .collect();
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }

    pub fn list_names(&self) -> Vec<String> {
        self.sources
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    #[allow(dead_code)]
    pub fn contains(&self, name: &str) -> bool {
        self.sources
            .read()
            .map(|s| s.contains_key(name))
            .unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.sources.read().map(|s| s.len()).unwrap_or(0)
    }

    #[allow(dead_code)]
    pub fn subscribe(&self, callback: EventCallback) {
        if let Ok(mut subs) = self.subscribers.write() {
            subs.push(callback);
        }
    }

    fn notify(&self, event: RegistryEvent) {
        if let Ok(subs) = self.subscribers.read() {
            for cb in subs.iter() {
                cb(event.clone());
            }
        }
    }
}

impl Default for DataSourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}
