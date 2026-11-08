//! Pictures for notifications: a rule's Notify step can carry one from a
//! device that keeps its newest in its `snapshot` attribute (a camera). The
//! executor asks the device for a fresh one (`take_snapshot`), waits for the
//! attribute to name it, and fetches the file from the device's plugin
//! through this — a plugin's files are only reachable over its management
//! channel, which the API layer holds.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

pub type FetchFuture = Pin<Box<dyn Future<Output = anyhow::Result<Vec<u8>>> + Send>>;

/// A device's files, kept by its plugin, fetched by path
/// (`snap/reolink_porch-1794.jpg`): the fetcher finds the plugin.
pub trait PluginFiles: Send + Sync {
    fn fetch(&self, device_id: &str, path: &str) -> FetchFuture;
}

/// Filled once the API layer can ask plugins; until then a Notify step with
/// a picture sends its words alone.
pub type PluginFilesSlot = Arc<OnceLock<Arc<dyn PluginFiles>>>;

/// A picture's type from its file name; `None` for one we don't send.
pub fn mime_of(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::mime_of;

    #[test]
    fn names_a_pictures_type_by_its_file() {
        assert_eq!(mime_of("snap/reolink_porch-1.jpg"), Some("image/jpeg"));
        assert_eq!(mime_of("a.PNG"), Some("image/png"));
        assert_eq!(mime_of("notes.txt"), None);
        assert_eq!(mime_of("noext"), None);
    }
}
