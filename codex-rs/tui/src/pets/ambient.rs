//! Ambient terminal rendering for the Codex companion.
//!
//! Ambient pets reuse the same extracted image frames as the full-screen viewer
//! but are rendered through a different ownership split: ratatui still owns the
//! transcript/composer layout, while the sprite itself is emitted through the
//! terminal image protocol after the frame draw completes.
//!
//! This module therefore owns two separate contracts:
//! choosing which animation frame should be visible for the current semantic
//! pet state, and translating that frame into a precise on-screen image request
//! that does not overlap reserved bottom-pane space. It does not persist pet
//! selection or decide when modal/popover UI should suppress the sprite.

#[cfg(test)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::tui::FrameRequester;

use super::BONGO_CAT_PET_ID;
use super::DEFAULT_PET_ID;
use super::bongo::BONGO_CAT_HEIGHT;
use super::bongo::BONGO_CAT_WIDTH;
use super::bongo::BongoCat;
use super::bongo::MIN_BONGO_TERMINAL_WIDTH;
use super::frames;
use super::image_protocol::ImageProtocol;
use super::image_protocol::PetImageSupport;
use super::model::Animation;
#[cfg(test)]
use super::model::AnimationFrame;
use super::model::Pet;

const PET_TARGET_HEIGHT_PX: u16 = 75;
const PET_COMPOSER_GAP_PX: u16 = 10;
const TERMINAL_ROW_HEIGHT_PX: u16 = 15;

const RUNNING_LIFETIME: Duration = Duration::from_secs(3 * 60);
const FAILED_LIFETIME: Duration = Duration::from_secs(60 * 60);
const WAITING_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const REVIEW_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PetNotificationKind {
    Running,
    Waiting,
    Review,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AmbientPetActivity {
    Idle,
    Typing { idle_in: Duration },
}

impl PetNotificationKind {
    fn animation_name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Review => "review",
            Self::Failed => "failed",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Waiting => "Needs input",
            Self::Review => "Ready",
            Self::Failed => "Blocked",
        }
    }

    fn fallback_body(self) -> &'static str {
        match self {
            Self::Running => "Thinking",
            Self::Waiting => "Needs input",
            Self::Review => "Ready",
            Self::Failed => "Blocked",
        }
    }

    fn lifetime(self) -> Duration {
        match self {
            Self::Running => RUNNING_LIFETIME,
            Self::Waiting => WAITING_LIFETIME,
            Self::Review => REVIEW_LIFETIME,
            Self::Failed => FAILED_LIFETIME,
        }
    }
}

#[derive(Debug, Clone)]
struct PetNotification {
    kind: PetNotificationKind,
    body: String,
    updated_at: Instant,
}

impl PetNotification {
    fn new(kind: PetNotificationKind, body: Option<String>) -> Self {
        Self {
            kind,
            body: body.unwrap_or_else(|| kind.fallback_body().to_string()),
            updated_at: Instant::now(),
        }
    }

    fn is_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.updated_at) >= self.kind.lifetime()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AmbientPetDraw {
    pub(crate) frame: PathBuf,
    pub(crate) protocol: ImageProtocol,
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) clear_top_y: u16,
    pub(crate) columns: u16,
    pub(crate) rows: u16,
    pub(crate) height_px: u16,
    pub(crate) sixel_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct AmbientPet {
    kind: AmbientPetKind,
}

#[derive(Debug)]
enum AmbientPetKind {
    Sprite(Box<SpritePet>),
    BongoAscii(BongoCat),
}

#[derive(Debug)]
struct SpritePet {
    pet: Pet,
    support: PetImageSupport,
    frames: Vec<PathBuf>,
    sixel_dir: PathBuf,
    frame_requester: FrameRequester,
    notification: Option<PetNotification>,
    animation_started_at: Instant,
    typing_activity: Option<TypingActivity>,
    animations_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypingActivity {
    started_at: Instant,
    idle_at: Instant,
}

impl AmbientPet {
    pub(crate) fn load(
        selected_pet: Option<&str>,
        codex_home: &std::path::Path,
        frame_requester: FrameRequester,
        animations_enabled: bool,
        support: PetImageSupport,
    ) -> Result<Self> {
        if selected_pet == Some(BONGO_CAT_PET_ID) && support.protocol().is_none() {
            return Ok(Self {
                kind: AmbientPetKind::BongoAscii(BongoCat::new(
                    frame_requester,
                    animations_enabled,
                )),
            });
        }

        SpritePet::load(
            selected_pet,
            codex_home,
            frame_requester,
            animations_enabled,
            support,
        )
        .map(|pet| Self {
            kind: AmbientPetKind::Sprite(Box::new(pet)),
        })
    }

    pub(crate) fn set_notification(&mut self, kind: PetNotificationKind, body: Option<String>) {
        if let AmbientPetKind::Sprite(pet) = &mut self.kind {
            pet.set_notification(kind, body);
        }
    }

    pub(crate) fn image_enabled(&self) -> bool {
        match &self.kind {
            AmbientPetKind::Sprite(pet) => pet.image_enabled(),
            AmbientPetKind::BongoAscii(_) => false,
        }
    }

    pub(crate) fn layout_columns(&self) -> Option<u16> {
        match &self.kind {
            AmbientPetKind::Sprite(pet) => pet.image_enabled().then(|| pet.image_columns()),
            AmbientPetKind::BongoAscii(_) => Some(BONGO_CAT_WIDTH),
        }
    }

    /// True when the pet is deliberately hidden at this terminal width.
    ///
    /// The ASCII Bongo Cat is not rendered (and reserves no layout space) when
    /// the terminal is narrower than [`MIN_BONGO_TERMINAL_WIDTH`].
    pub(crate) fn hidden_at_width(&self, width: u16) -> bool {
        match &self.kind {
            AmbientPetKind::Sprite(_) => false,
            AmbientPetKind::BongoAscii(_) => width < MIN_BONGO_TERMINAL_WIDTH,
        }
    }

    pub(crate) fn text_height(&self) -> Option<u16> {
        match &self.kind {
            AmbientPetKind::Sprite(_) => None,
            AmbientPetKind::BongoAscii(_) => Some(BONGO_CAT_HEIGHT),
        }
    }

    pub(crate) fn is_ascii_bongo(&self) -> bool {
        matches!(&self.kind, AmbientPetKind::BongoAscii(_))
    }

    #[cfg(test)]
    pub(crate) fn set_image_support_for_tests(&mut self, support: PetImageSupport) {
        if let AmbientPetKind::Sprite(pet) = &mut self.kind {
            pet.set_image_support_for_tests(support);
        }
    }

    pub(crate) fn set_activity_at(&mut self, activity: AmbientPetActivity, now: Instant) {
        match &mut self.kind {
            AmbientPetKind::Sprite(pet) => pet.set_activity_at(activity, now),
            AmbientPetKind::BongoAscii(pet) => pet.set_activity_at(activity, now),
        }
    }

    pub(crate) fn schedule_next_frame_at(&self, now: Instant) {
        match &self.kind {
            AmbientPetKind::Sprite(pet) => pet.schedule_next_frame_at(now),
            AmbientPetKind::BongoAscii(pet) => pet.schedule_next_frame_at(now),
        }
    }

    pub(crate) fn draw_request(
        &self,
        area: Rect,
        composer_bottom_y: u16,
    ) -> Option<AmbientPetDraw> {
        match &self.kind {
            AmbientPetKind::Sprite(pet) => pet.draw_request(area, composer_bottom_y),
            AmbientPetKind::BongoAscii(_) => None,
        }
    }

    pub(crate) fn preview_draw_request(&self, area: Rect) -> Option<AmbientPetDraw> {
        match &self.kind {
            AmbientPetKind::Sprite(pet) => pet.preview_draw_request(area),
            AmbientPetKind::BongoAscii(_) => None,
        }
    }

    pub(crate) fn render_text(&self, area: Rect, anchor_bottom_y: u16, buf: &mut Buffer) -> bool {
        match &self.kind {
            AmbientPetKind::Sprite(_) => false,
            AmbientPetKind::BongoAscii(pet) => pet.render(area, anchor_bottom_y, buf),
        }
    }
}

impl SpritePet {
    /// Load the active ambient pet and prepare its frame cache.
    ///
    /// This resolves the selected pet id, extracts per-frame PNGs into the
    /// CODEX_HOME cache, and records the terminal protocol support snapshot used
    /// for later draw requests. A caller that repeatedly recreates `AmbientPet`
    /// instead of mutating one instance would lose animation timing continuity
    /// and pay the frame-cache preparation cost more often than necessary.
    fn load(
        selected_pet: Option<&str>,
        codex_home: &std::path::Path,
        frame_requester: FrameRequester,
        animations_enabled: bool,
        support: PetImageSupport,
    ) -> Result<Self> {
        let pet = Pet::load_with_codex_home(
            selected_pet.unwrap_or(DEFAULT_PET_ID),
            /*codex_home*/ Some(codex_home),
        )
        .with_context(|| "load ambient pet")?;
        let cache_dir = codex_home
            .join("cache")
            .join("tui-pets")
            .join("frame-cache")
            .join(&pet.id)
            .join(pet.frame_cache_key()?);
        let frame_dir = cache_dir.join("frames");
        let sixel_dir = cache_dir.join("sixel");
        let frames = frames::prepare_png_frames(&pet, &frame_dir)?;
        Ok(Self {
            pet,
            support,
            frames,
            sixel_dir,
            frame_requester,
            notification: None,
            animation_started_at: Instant::now(),
            typing_activity: None,
            animations_enabled,
        })
    }

    pub(crate) fn set_notification(&mut self, kind: PetNotificationKind, body: Option<String>) {
        self.notification = Some(PetNotification::new(kind, body));
        self.animation_started_at = Instant::now();
    }

    pub(crate) fn image_enabled(&self) -> bool {
        self.support.protocol().is_some()
    }

    pub(crate) fn image_columns(&self) -> u16 {
        self.image_size().columns
    }

    fn set_activity_at(&mut self, activity: AmbientPetActivity, now: Instant) {
        let idle_at = match activity {
            AmbientPetActivity::Typing { idle_in }
                if self.animations_enabled
                    && !idle_in.is_zero()
                    && self.pet.animations.contains_key("typing") =>
            {
                now.checked_add(idle_in)
            }
            AmbientPetActivity::Idle | AmbientPetActivity::Typing { .. } => None,
        };
        let Some(idle_at) = idle_at else {
            self.typing_activity = None;
            return;
        };

        let started_at = self
            .active_typing_activity(now)
            .map_or(now, |activity| activity.started_at);
        self.typing_activity = Some(TypingActivity {
            started_at,
            idle_at,
        });
    }

    #[cfg(test)]
    pub(crate) fn set_image_support_for_tests(&mut self, support: PetImageSupport) {
        self.support = support;
    }

    fn schedule_next_frame_at(&self, now: Instant) {
        if let Some(delay) = self.next_frame_delay_at(now) {
            self.frame_requester.schedule_frame_in(delay);
        }
    }

    fn next_frame_delay_at(&self, now: Instant) -> Option<Duration> {
        if self.support.protocol().is_none() || !self.animations_enabled {
            return None;
        }

        let (animation, started_at) = self.current_animation_at(now)?;
        let delay =
            current_animation_frame(animation, now.saturating_duration_since(started_at))?.delay?;
        Some(self.active_typing_activity(now).map_or(delay, |activity| {
            delay.min(activity.idle_at.saturating_duration_since(now))
        }))
    }

    /// Build an image draw request for the ambient pet anchored above the composer.
    ///
    /// Returning `None` means "do not render the sprite this frame", typically
    /// because the terminal protocol is unavailable or the current layout cannot
    /// fit the image without overlapping reserved UI. Callers should not try to
    /// partially clip the image themselves; that would desynchronize the image
    /// protocol output from the TUI's notion of cleared rows.
    pub(crate) fn draw_request(
        &self,
        area: Rect,
        composer_bottom_y: u16,
    ) -> Option<AmbientPetDraw> {
        let protocol = self.support.protocol()?;
        let size = self.image_size();
        let now = Instant::now();
        let notification = self.visible_notification(now);
        let notification_height = notification.map_or(0, notification_height);
        let required_height = size.rows.saturating_add(notification_height);
        let sprite_bottom_y = composer_bottom_y.saturating_sub(composer_gap_rows());
        if sprite_bottom_y < area.y.saturating_add(required_height) || area.width < size.columns {
            return None;
        }

        let x = area.x + area.width.saturating_sub(size.columns);
        let y = sprite_bottom_y.saturating_sub(size.rows);
        Some(AmbientPetDraw {
            frame: self.current_frame_path_at(now)?,
            protocol,
            x,
            y,
            clear_top_y: area.y,
            columns: size.columns,
            rows: size.rows,
            height_px: size.height_px,
            sixel_dir: self.sixel_dir.clone(),
        })
    }

    /// Build a centered preview draw request for the `/pets` picker side pane.
    ///
    /// The picker preview intentionally uses the first idle frame rather than
    /// the live animation state so selection browsing stays stable and does not
    /// require the full ambient animation lifecycle.
    pub(crate) fn preview_draw_request(&self, area: Rect) -> Option<AmbientPetDraw> {
        let protocol = self.support.protocol()?;
        let size = self.image_size();
        if area.width < size.columns || area.height < size.rows {
            return None;
        }

        let y = area.y + area.height.saturating_sub(size.rows) / 2;
        Some(AmbientPetDraw {
            frame: self.first_idle_frame_path()?,
            protocol,
            x: area.x + area.width.saturating_sub(size.columns) / 2,
            y,
            clear_top_y: y,
            columns: size.columns,
            rows: size.rows,
            height_px: size.height_px,
            sixel_dir: self.sixel_dir.clone(),
        })
    }

    fn visible_notification(&self, now: Instant) -> Option<&PetNotification> {
        self.notification
            .as_ref()
            .filter(|notification| !notification.is_expired(now))
    }

    fn active_typing_activity(&self, now: Instant) -> Option<TypingActivity> {
        self.typing_activity
            .filter(|activity| now < activity.idle_at)
    }

    fn current_animation_at(&self, now: Instant) -> Option<(&Animation, Instant)> {
        if let Some(activity) = self.active_typing_activity(now)
            && let Some(animation) = self.pet.animations.get("typing")
        {
            return Some((animation, activity.started_at));
        }

        let animation_name = self
            .visible_notification(now)
            .map_or("idle", |notification| notification.kind.animation_name());
        let animation = self
            .pet
            .animations
            .get(animation_name)
            .or_else(|| self.pet.animations.get("idle"))?;
        if animation.loop_start.is_none() {
            let elapsed = now.saturating_duration_since(self.animation_started_at);
            if elapsed >= animation.total_duration()
                && let Some(fallback) = self.pet.animations.get(&animation.fallback)
            {
                return Some((fallback, self.animation_started_at));
            }
        }
        Some((animation, self.animation_started_at))
    }

    fn current_frame_path_at(&self, now: Instant) -> Option<PathBuf> {
        let sprite_index = self
            .current_animation_at(now)
            .and_then(|(animation, started_at)| {
                if self.animations_enabled {
                    current_animation_frame(animation, now.saturating_duration_since(started_at))
                        .map(|frame| frame.sprite_index)
                } else {
                    animation.frames.first().map(|frame| frame.sprite_index)
                }
            })
            .unwrap_or(0);
        self.frame_path_for_sprite_index(sprite_index)
    }

    fn first_idle_frame_path(&self) -> Option<PathBuf> {
        let sprite_index = self
            .pet
            .animations
            .get("idle")
            .and_then(|animation| animation.frames.first())
            .map_or(0, |frame| frame.sprite_index);
        self.frame_path_for_sprite_index(sprite_index)
    }

    fn frame_path_for_sprite_index(&self, sprite_index: usize) -> Option<PathBuf> {
        self.frames
            .get(sprite_index.min(self.frames.len().saturating_sub(1)))
            .cloned()
    }

    fn image_size(&self) -> ImageSize {
        let rows = (f64::from(PET_TARGET_HEIGHT_PX) / f64::from(TERMINAL_ROW_HEIGHT_PX))
            .round()
            .max(/*other*/ 1.0) as u16;
        let aspect = f64::from(self.pet.frame_height) / f64::from(self.pet.frame_width) * 0.52;
        let columns = (f64::from(rows) / aspect).round() as u16;
        ImageSize {
            columns: columns.max(1),
            rows,
            height_px: PET_TARGET_HEIGHT_PX,
        }
    }
}

fn composer_gap_rows() -> u16 {
    ((f64::from(PET_COMPOSER_GAP_PX) / f64::from(TERMINAL_ROW_HEIGHT_PX)).round() as u16)
        .max(/*other*/ 1)
}

#[derive(Debug, Clone, Copy)]
struct ImageSize {
    columns: u16,
    rows: u16,
    height_px: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnimationFrameTick {
    sprite_index: usize,
    delay: Option<Duration>,
}

fn current_animation_frame(animation: &Animation, elapsed: Duration) -> Option<AnimationFrameTick> {
    if animation.frames.len() <= 1 {
        return Some(AnimationFrameTick {
            sprite_index: animation.frames.first()?.sprite_index,
            delay: None,
        });
    }

    let elapsed_nanos = elapsed.as_nanos();
    if let Some(loop_start) = animation
        .loop_start
        .filter(|idx| *idx < animation.frames.len())
    {
        let total_nanos = animation.total_duration().as_nanos();
        let prefix_nanos = animation.frames[..loop_start]
            .iter()
            .map(|frame| frame.duration.as_nanos())
            .sum::<u128>();
        let loop_nanos = animation.frames[loop_start..]
            .iter()
            .map(|frame| frame.duration.as_nanos())
            .sum::<u128>();
        let effective_elapsed = if elapsed_nanos >= total_nanos && loop_nanos > 0 {
            prefix_nanos + elapsed_nanos.saturating_sub(prefix_nanos) % loop_nanos
        } else {
            elapsed_nanos
        };
        frame_at_elapsed(animation, effective_elapsed)
    } else if elapsed_nanos >= animation.total_duration().as_nanos() {
        Some(AnimationFrameTick {
            sprite_index: animation.frames.last()?.sprite_index,
            delay: None,
        })
    } else {
        frame_at_elapsed(animation, elapsed_nanos)
    }
}

fn frame_at_elapsed(animation: &Animation, elapsed_nanos: u128) -> Option<AnimationFrameTick> {
    let mut remaining_elapsed = elapsed_nanos;
    for frame in &animation.frames {
        let frame_nanos = frame.duration.as_nanos().max(/*other*/ 1);
        if remaining_elapsed < frame_nanos {
            return Some(AnimationFrameTick {
                sprite_index: frame.sprite_index,
                delay: Some(nanos_to_duration(frame_nanos - remaining_elapsed)),
            });
        }
        remaining_elapsed = remaining_elapsed.saturating_sub(frame_nanos);
    }

    Some(AnimationFrameTick {
        sprite_index: animation.frames.last()?.sprite_index,
        delay: None,
    })
}

fn nanos_to_duration(nanos: u128) -> Duration {
    Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

fn notification_height(notification: &PetNotification) -> u16 {
    if notification.body == notification.kind.label() {
        1
    } else {
        2
    }
}

#[cfg(test)]
pub(crate) fn test_ambient_pet(
    frame_requester: FrameRequester,
    animations_enabled: bool,
) -> AmbientPet {
    AmbientPet {
        kind: AmbientPetKind::Sprite(Box::new(SpritePet {
            pet: Pet {
                id: "test".to_string(),
                display_name: "Test".to_string(),
                description: String::new(),
                spritesheet_path: PathBuf::from("spritesheet.webp"),
                frame_width: 192,
                frame_height: 208,
                columns: 8,
                rows: 9,
                frame_count: 72,
                animations: HashMap::from([("idle".to_string(), test_animation())]),
            },
            support: PetImageSupport::Supported(ImageProtocol::Kitty),
            frames: vec![PathBuf::from("frame-0.png"), PathBuf::from("frame-1.png")],
            sixel_dir: PathBuf::new(),
            frame_requester,
            notification: None,
            animation_started_at: Instant::now()
                .checked_sub(Duration::from_millis(/*millis*/ 15))
                .unwrap(),
            typing_activity: None,
            animations_enabled,
        })),
    }
}

#[cfg(test)]
fn test_animation() -> Animation {
    Animation {
        frames: vec![
            AnimationFrame {
                sprite_index: 0,
                duration: Duration::from_millis(/*millis*/ 10),
            },
            AnimationFrame {
                sprite_index: 1,
                duration: Duration::from_millis(/*millis*/ 10),
            },
        ],
        loop_start: Some(/*loop_start*/ 0),
        fallback: "idle".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_labels_match_codex_app_vocabulary() {
        assert_eq!(PetNotificationKind::Running.label(), "Running");
        assert_eq!(PetNotificationKind::Waiting.label(), "Needs input");
        assert_eq!(PetNotificationKind::Review.label(), "Ready");
        assert_eq!(PetNotificationKind::Failed.label(), "Blocked");
    }

    #[test]
    fn ascii_bongo_hides_below_min_terminal_width() {
        let pet = AmbientPet {
            kind: AmbientPetKind::BongoAscii(BongoCat::new(
                FrameRequester::test_dummy(),
                /*animations_enabled*/ false,
            )),
        };
        assert!(pet.hidden_at_width(MIN_BONGO_TERMINAL_WIDTH - 1));
        assert!(!pet.hidden_at_width(MIN_BONGO_TERMINAL_WIDTH));
        assert!(!pet.hidden_at_width(u16::MAX));

        let narrow_area = Rect::new(0, 0, MIN_BONGO_TERMINAL_WIDTH - 1, 20);
        let mut narrow = Buffer::empty(narrow_area);
        assert!(!pet.render_text(narrow_area, /*anchor_bottom_y*/ 12, &mut narrow));

        let wide_area = Rect::new(0, 0, MIN_BONGO_TERMINAL_WIDTH, 20);
        let mut wide = Buffer::empty(wide_area);
        assert!(pet.render_text(wide_area, /*anchor_bottom_y*/ 12, &mut wide));
    }

    #[test]
    fn animation_frame_uses_per_frame_duration() {
        let animation = test_animation();

        assert_eq!(
            current_animation_frame(&animation, Duration::from_millis(/*millis*/ 15)),
            Some(AnimationFrameTick {
                sprite_index: 1,
                delay: Some(Duration::from_millis(/*millis*/ 5)),
            })
        );
    }
}
