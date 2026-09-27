//! Explicit native reads only. OSC 52 writes and terminal paste remain separate.
//!
//! arboard 3.6.1 supports clipboard instances on non-main threads on macOS,
//! Windows, X11 and Wayland. Windows operations must not run concurrently.
//! The global permit survives UI/session replacement and timeout: a blocked
//! native call never allows another replacement thread to enter the backend.
use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;
use zevria_content::PromptImage;

static NATIVE_ACTIVE: LazyLock<Arc<AtomicBool>> =
    LazyLock::new(|| Arc::new(AtomicBool::new(false)));
pub(crate) const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
pub(crate) enum ClipboardResult {
    Image(PromptImage),
    Text(String),
    Empty,
    Error(String),
}

pub(crate) struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}
pub(crate) trait ClipboardBackend {
    fn image(&mut self) -> Result<Option<Bitmap>, ()>;
    fn text(&mut self) -> Result<Option<String>, ()>;
}
struct Native(arboard::Clipboard);
impl ClipboardBackend for Native {
    fn image(&mut self) -> Result<Option<Bitmap>, ()> {
        match self.0.get_image() {
            Ok(image) => Ok(Some(Bitmap {
                width: image.width,
                height: image.height,
                rgba: image.bytes.into_owned(),
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(_) => Err(()),
        }
    }
    fn text(&mut self) -> Result<Option<String>, ()> {
        match self.0.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(_) => Err(()),
        }
    }
}
pub(crate) fn read(backend: &mut impl ClipboardBackend) -> ClipboardResult {
    let image = backend.image();
    if let Ok(Some(bitmap)) = image {
        return match PromptImage::from_rgba(bitmap.width, bitmap.height, &bitmap.rgba) {
            Ok(image) => ClipboardResult::Image(image),
            Err(error) => ClipboardResult::Error(error.to_string()),
        };
    }
    match backend.text() {
        Ok(Some(text)) if !text.is_empty() => ClipboardResult::Text(text),
        Ok(_) if image.is_ok() => ClipboardResult::Empty,
        _ => ClipboardResult::Error("Could not read image or text from the native clipboard. Check desktop/SSH access and retry.".into()),
    }
}

pub(crate) struct ClipboardService {
    active: Arc<AtomicBool>,
    sender: mpsc::Sender<(u64, ClipboardResult)>,
    pub receiver: mpsc::Receiver<(u64, ClipboardResult)>,
}
impl ClipboardService {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(1);
        Self {
            active: NATIVE_ACTIVE.clone(),
            sender,
            receiver,
        }
    }
    pub fn start(&self, id: u64) -> bool {
        self.start_operation(id, || match arboard::Clipboard::new() {
            Ok(clipboard) => read(&mut Native(clipboard)),
            Err(_) => ClipboardResult::Error(
                "Native clipboard is unavailable in this desktop session.".into(),
            ),
        })
    }
    fn start_operation(
        &self,
        id: u64,
        operation: impl FnOnce() -> ClipboardResult + Send + 'static,
    ) -> bool {
        if self
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let active = self.active.clone();
        let sender = self.sender.clone();
        let result = std::thread::Builder::new()
            .name("zevria-clipboard".into())
            .spawn(move || {
                struct Permit(Arc<AtomicBool>);
                impl Drop for Permit {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _permit = Permit(active);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
                    .unwrap_or_else(|_| {
                        ClipboardResult::Error("Native clipboard operation failed.".into())
                    });
                let _ = sender.try_send((id, result));
            });
        if result.is_err() {
            self.active.store(false, Ordering::Release);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        image: Result<Option<Bitmap>, ()>,
        text: Result<Option<String>, ()>,
        text_reads: usize,
    }
    impl ClipboardBackend for Fake {
        fn image(&mut self) -> Result<Option<Bitmap>, ()> {
            std::mem::replace(&mut self.image, Ok(None))
        }
        fn text(&mut self) -> Result<Option<String>, ()> {
            self.text_reads += 1;
            self.text.clone()
        }
    }
    #[test]
    fn image_preferred_and_validation_does_not_fall_back() {
        let mut fake = Fake {
            image: Ok(Some(Bitmap {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
            })),
            text: Ok(Some("text".into())),
            text_reads: 0,
        };
        assert!(matches!(read(&mut fake), ClipboardResult::Image(_)));
        assert_eq!(fake.text_reads, 0);
        fake.image = Ok(Some(Bitmap {
            width: 0,
            height: 1,
            rgba: vec![],
        }));
        assert!(matches!(read(&mut fake), ClipboardResult::Error(_)));
        assert_eq!(fake.text_reads, 0);
    }
    #[tokio::test]
    async fn blocked_backend_retains_the_single_permit_after_receiver_replacement() {
        let mut service = ClipboardService::new();
        service.active = Arc::new(AtomicBool::new(false));
        let (release, wait) = std::sync::mpsc::channel();
        assert!(service.start_operation(1, move || {
            wait.recv().unwrap();
            ClipboardResult::Text("late".into())
        }));
        assert!(!service.start_operation(2, || panic!("second backend must not start")));
        let active = service.active.clone();
        drop(service); // Session retirement/timeout does not free the native permit.
        let mut replacement = ClipboardService::new();
        replacement.active = active.clone();
        assert!(!replacement.start_operation(3, || panic!("old backend still active")));
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while active.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(replacement.start_operation(4, || ClipboardResult::Empty));
        let (id, result) = replacement.receiver.recv().await.unwrap();
        assert_eq!(id, 4);
        assert!(matches!(result, ClipboardResult::Empty));
    }

    #[test]
    fn text_fallback_after_absence_and_error() {
        for image in [Ok(None), Err(())] {
            let mut fake = Fake {
                image,
                text: Ok(Some("/path/is/text".into())),
                text_reads: 0,
            };
            assert!(
                matches!(read(&mut fake), ClipboardResult::Text(text) if text == "/path/is/text")
            );
            assert_eq!(fake.text_reads, 1);
        }
        let mut fake = Fake {
            image: Ok(None),
            text: Ok(None),
            text_reads: 0,
        };
        assert!(matches!(read(&mut fake), ClipboardResult::Empty));
        fake.image = Err(());
        assert!(matches!(read(&mut fake), ClipboardResult::Error(_)));
    }
}
