//! Reading and showing the project from a thread.
//!
//! The containment rule is `crate::sandbox::paths`. This is the session half:
//! resolving what somebody typed, deciding what is small enough to show,
//! reporting an edit as a diff rather than as a name, and posting an image the
//! agent made as an upload rather than as a path.

use std::fmt::Write as _;

use super::{MAX_DIFFABLE_BYTES, MAX_IMAGES_PER_TURN, MAX_INLINE, Running};
use crate::agent::protocol::ProducedFile;
use crate::chat::diff::file_diff;
use crate::chat::render::{bytes as byte_count, directory_listing, file_view, widen};
use crate::log::{LogValue, fields};
use crate::sandbox::paths;
use crate::session::event::SessionEvent;
use crate::session::files::{NotAFileError, read_directory, read_file_for_display};

impl Running {
    /// Records a file's contents before an edit, so the change can be shown.
    /// Reads a project file, refusing a link however the path is spelled.
    ///
    /// The agent writes this tree, so a file the daemon is about to read can
    /// have become a link to somewhere else since it was last looked at.
    fn read_contained(&self, agent_path: &str) -> std::io::Result<String> {
        let display = self.display_path(agent_path);
        paths::read_beneath(&self.options.project.path, &display)
    }

    pub(super) fn snapshot(&mut self, agent_path: &str) {
        let Some(host) = self.host_path(agent_path) else {
            return;
        };
        match std::fs::metadata(&host) {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_DIFFABLE_BYTES => {
                if let Ok(text) = self.read_contained(agent_path) {
                    self.remember_edit(agent_path, text);
                }
            }
            // A file that does not exist yet is an empty one for diffing
            // purposes.
            _ => self.remember_edit(agent_path, String::new()),
        }
    }

    pub(super) fn remember_edit(&mut self, agent_path: &str, before: String) {
        if let Some(existing) = self
            .pending_edits
            .iter_mut()
            .find(|(path, _)| path == agent_path)
        {
            existing.1 = before;
        } else {
            self.pending_edits.push((agent_path.to_owned(), before));
        }
    }

    /// Posts what an edit changed, once the tool has finished.
    pub(super) async fn report_edit(&mut self, tool_name: &str, call: &str) {
        if !self.options.config.output.post_diffs {
            self.pending_edits.clear();
            return;
        }

        let edits = std::mem::take(&mut self.pending_edits);
        for (agent_path, before) in edits {
            let Some(host) = self.host_path(&agent_path) else {
                continue;
            };
            let Ok(meta) = std::fs::metadata(&host) else {
                continue;
            };
            if !meta.is_file() || meta.len() > MAX_DIFFABLE_BYTES {
                continue;
            }
            let Ok(after) = self.read_contained(&agent_path) else {
                continue;
            };

            let diff = file_diff(&before, &after);
            if diff.empty {
                continue;
            }
            self.log.info(
                "posting an edit",
                &fields([
                    ("tool", LogValue::from(tool_name)),
                    ("path", LogValue::from(agent_path.as_str())),
                ]),
            );
            self.views
                .send(SessionEvent::Diff {
                    path: self.display_path(&agent_path),
                    added: diff.added as u64,
                    removed: diff.removed as u64,
                    body: diff.body,
                    cause: Some(call.to_owned()),
                })
                .await;
        }
    }

    pub(super) fn host_path(&self, agent_path: &str) -> Option<String> {
        self.sandbox
            .as_ref()
            .and_then(|sandbox| (sandbox.to_host_path)(agent_path))
    }

    /// A path as a reader would recognise it, relative to the project.
    pub(super) fn display_path(&self, agent_path: &str) -> String {
        let Some(host) = self.host_path(agent_path) else {
            return agent_path.to_owned();
        };
        let root = &self.options.project.path;
        match host.strip_prefix(root) {
            Some(rest) => rest.trim_start_matches('/').to_owned(),
            None => host,
        }
    }

    /// Lists a directory or shows a file, without involving the agent.
    pub(super) async fn read_path(&mut self, request: &str) {
        let wanted = request.trim();
        let wanted = if wanted.is_empty() { "." } else { wanted };
        let root = self.options.project.path.clone();
        let Some(host) = self.host_path(wanted) else {
            self.say(&format!("`{wanted}` is not inside this session's project"))
                .await;
            return;
        };

        let display = {
            let shown = self.display_path(wanted);
            if shown.is_empty() {
                ".".to_owned()
            } else {
                shown
            }
        };
        let result = std::fs::metadata(&host).map(|meta| meta.is_dir());
        match result {
            Ok(true) => {
                // A directory is listed whichever was asked for: `!cat` on
                // one is a mistake worth answering rather than an error worth
                // reporting.
                match read_directory(&root, &display) {
                    Ok(entries) => {
                        self.say(&directory_listing(&entries, &display)).await;
                    }
                    Err(error) => {
                        self.say(&format!("could not read `{wanted}`: {error}"))
                            .await;
                    }
                }
            }
            Ok(false) => match read_file_for_display(&root, &display, MAX_INLINE) {
                Ok(contents) => {
                    let shown = file_view(&contents);
                    self.say(&shown).await;
                }
                Err(NotAFileError(path)) => {
                    self.say(&format!("could not read `{wanted}`: {path} is not a file"))
                        .await;
                }
            },
            Err(error) => {
                self.say(&format!("could not read `{wanted}`: {error}"))
                    .await;
            }
        }
    }

    /// Uploads a file from the project on request.
    ///
    /// Opened beneath the project, the same rule `read_path` answers `!cat`
    /// with. This used to be the one read that went by name: a link planted
    /// where the agent writes would have had the daemon read whatever it
    /// pointed at and post it to the channel.
    pub(super) async fn upload_file(&mut self, request: &str) {
        use std::io::Read;

        let wanted = request.trim();
        if wanted.is_empty() {
            self.say("say which file, as `!file <path>`").await;
            return;
        }

        let root = self.options.project.path.clone();
        let Some(host) = self.host_path(wanted) else {
            self.say(&format!("`{wanted}` is not inside this session's project"))
                .await;
            return;
        };

        let display = {
            let shown = self.display_path(wanted);
            if shown.is_empty() {
                ".".to_owned()
            } else {
                shown
            }
        };
        let mut file = match paths::open_beneath(&root, &display, &paths::OpenOptions::read()) {
            Ok(file) => file,
            Err(error) => {
                self.say(&format!("could not read `{wanted}`: {error}"))
                    .await;
                return;
            }
        };
        // Measured on the file that was opened, so what is checked and what is
        // sent are the same however the tree changes underneath.
        let meta = match file.metadata() {
            Ok(meta) => meta,
            Err(error) => {
                self.say(&format!("could not read `{wanted}`: {error}"))
                    .await;
                return;
            }
        };
        if !meta.is_file() {
            self.say(&format!("`{wanted}` is not a file")).await;
            return;
        }
        let limit = self.options.config.output.max_upload_bytes;
        if meta.len() > limit {
            #[expect(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let kilobytes = (meta.len() as f64 / 1024.0).round() as u64;
            self.say(&format!(
                "`{wanted}` is {kilobytes} KB, larger than the {limit} byte upload limit"
            ))
            .await;
            return;
        }
        let mut bytes = Vec::new();
        if let Err(error) = file.read_to_end(&mut bytes) {
            self.say(&format!("could not read `{wanted}`: {error}"))
                .await;
            return;
        }
        let name = host.rsplit('/').next().unwrap_or("file").to_owned();
        self.views
            .send(SessionEvent::Upload {
                name,
                bytes,
                caption: format!("`{}` {} bytes", display, meta.len()),
            })
            .await;
    }

    /// Posts an image the agent made as an upload, captioned with where it
    /// is, its size, and what its server said about it. One that cannot be
    /// posted is reported in the thread with the reason instead, so somebody
    /// told that an image was saved is not left waiting for it. Either way it
    /// counts toward the turn's cap.
    pub(super) async fn post_image(&mut self, file: &ProducedFile) {
        if self.turn_images >= MAX_IMAGES_PER_TURN {
            if self.turn_images == MAX_IMAGES_PER_TURN {
                self.say(&format!(
                    "the agent made more than {MAX_IMAGES_PER_TURN} images this turn. The rest \
                     are in the project, and `!file` posts one"
                ))
                .await;
            }
            self.turn_images += 1;
            return;
        }
        self.turn_images += 1;
        match self.read_image(&file.uri) {
            Ok((display, bytes)) => {
                self.log.info(
                    "posting an image the agent made",
                    &fields([("path", LogValue::from(display.as_str()))]),
                );
                let name = display.rsplit('/').next().unwrap_or("image").to_owned();
                let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                let caption = image_caption(&display, widen(size), file.note.as_deref());
                self.views
                    .send(SessionEvent::Upload {
                        name,
                        bytes,
                        caption,
                    })
                    .await;
            }
            Err(refusal) => {
                self.log.warn(
                    "an image the agent made was not posted",
                    &fields([
                        ("uri", LogValue::from(file.uri.as_str())),
                        ("reason", LogValue::from(refusal.as_str())),
                    ]),
                );
                self.say(&refusal).await;
            }
        }
    }

    /// The project path and bytes of the image `uri` names, or the line that
    /// says why it cannot be posted.
    ///
    /// The agent names the file and writes the tree it sits in, so the path
    /// is untrusted: it is opened beneath the project, refusing a link however
    /// it is spelled, as `!file` opens one, and without waiting on a pipe
    /// planted at the name. What is posted is what was read from the file that
    /// was opened, never more than the upload limit, and of a type its own
    /// bytes declare, whatever the agent called it.
    fn read_image(&self, uri: &str) -> Result<(String, Vec<u8>), String> {
        use std::io::Read;

        let Some(agent_path) = agent_path_of(uri).filter(|path| self.host_path(path).is_some())
        else {
            return Err(
                "an image the agent made is not a file in this session's project, so it was \
                 not posted"
                    .to_owned(),
            );
        };
        let display = self.display_path(&agent_path);
        let unposted = |why: &str| format!("generated `{display}` was not posted: {why}");
        let limit = self.options.config.output.max_upload_bytes;
        let over = |size: u64| {
            format!(
                "generated `{display}` is {}, over the {} upload limit",
                byte_count(widen(size)),
                byte_count(widen(limit))
            )
        };
        let root = &self.options.project.path;
        let opened =
            paths::open_beneath(root, &display, &paths::OpenOptions::read()).map_err(|error| {
                if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) {
                    unposted("a link is in its path")
                } else {
                    unposted(&error.to_string())
                }
            })?;
        let meta = opened
            .metadata()
            .map_err(|error| unposted(&error.to_string()))?;
        if !meta.is_file() {
            return Err(unposted("it is not a file"));
        }
        if meta.len() > limit {
            return Err(over(meta.len()));
        }
        // No further than the limit allows: a file grown since it was measured
        // is refused rather than read whole.
        let mut bytes = Vec::new();
        opened
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| unposted(&error.to_string()))?;
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > limit {
            return Err(over(size));
        }
        if !is_image(&bytes) {
            return Err(unposted("it is not a PNG, JPEG, GIF or WebP image"));
        }
        Ok((display, bytes))
    }
}

/// The longest part of an image server's note shown in a caption.
const MAX_NOTE_CHARS: usize = 300;

/// The path a `file://` URI names, percent-decoded. Nothing for another
/// scheme, a URI naming a host other than this one, or one that does not
/// decode to UTF-8.
fn agent_path_of(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let raw = path.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        if raw[at] == b'%' {
            let hex = raw.get(at + 1..at + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            decoded.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            at += 3;
        } else {
            decoded.push(raw[at]);
            at += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Whether `bytes` declare themselves an image of a type a chat service shows
/// as a picture: PNG, JPEG, GIF or WebP.
fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

/// The caption an image the agent made is posted with: where it is, its
/// size, and in brackets what its server said, such as a fallback to another
/// model, so a reader sees it beside the picture.
///
/// The note is the server's free text: it is kept to printable ASCII on one
/// line and cut short, with `...` where it was cut, so it cannot carry
/// control characters or a wall of text into the thread.
fn image_caption(display: &str, size: f64, note: Option<&str>) -> String {
    let mut caption = format!("generated `{display}` {}", byte_count(size));
    let printable: String = note
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { ' ' })
        .collect();
    let whole = printable.split_whitespace().collect::<Vec<_>>().join(" ");
    let note = if whole.len() > MAX_NOTE_CHARS {
        format!("{}...", whole[..MAX_NOTE_CHARS].trim_end())
    } else {
        whole
    };
    if !note.is_empty() {
        let _ = write!(caption, " [{note}]");
    }
    caption
}
