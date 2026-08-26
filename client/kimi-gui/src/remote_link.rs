//! The connection to a remote, as the tab strip's third button sees it.
//!
//! There is no long-lived connection to a bridge daemon — every session
//! dials its own — so "connected" is not a thing the app can simply observe.
//! This is what stands in for it: the tunnel process (when the remote needs
//! one) plus a periodic `version` probe through it, folded into the three
//! states the button paints.
//!
//! ```text
//! blank    no tunnel running, nothing answered yet
//! yellow   the tunnel is up (or a probe is in flight) but nothing answers
//! green    the daemon answered — sessions can be opened
//! ```
//!
//! The button is drawn by [`link_button`]: the state color fills the whole
//! square and a chain link is inked over it, the two halves pulling apart
//! as the light goes blank — the icon survives its hue.
//!
//! Probing happens on a background thread with a short timeout, never on the
//! UI thread. The cadence is deliberately lazy once connected: a green light
//! that goes stale for a few seconds costs nothing, and the probe opens a
//! real TCP connection each time.

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use eframe::egui::emath::Rot2;
use eframe::egui::{self, Color32, Pos2, Rect, Response, Shape, Stroke, Ui, Vec2};

use crate::theme::{BarStyle, Colors};
use wire_client::bridge;
use wire_client::remotes::Remote;
use wire_client::tunnel::{Tunnel, TunnelState};

/// How long a probe may take. Short: it is one round trip to a daemon that
/// answers without touching disk, and a slow answer is a bad answer here.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often to re-probe while connected.
const PROBE_INTERVAL_UP: Duration = Duration::from_secs(15);

/// How often to re-probe while trying to connect — the daemon may still be
/// coming up behind a tunnel that just opened.
const PROBE_INTERVAL_TRYING: Duration = Duration::from_secs(2);

/// What the button paints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LinkLight {
    /// Nothing running: a click connects.
    Blank,
    /// Running but nothing answers yet.
    Trying,
    /// The daemon answered: a click opens a session.
    Connected,
}

impl LinkLight {
    /// The chain's fill: the whole button carries the state color. Same pair
    /// as the old dot, promoted from a glyph to the button itself.
    pub fn fill(self, colors: &Colors, weak: Color32) -> Color32 {
        match self {
            Self::Connected => colors.success,
            Self::Trying => colors.warning,
            Self::Blank => weak,
        }
    }

    /// The chain's stroke — ink against a filled button. The blank state's
    /// fill comes from the UI, so its "stroke" color is a readable no-op.
    pub fn ink(self, fill: Color32) -> Color32 {
        Color32::from_rgb(255 - fill.r(), 255 - fill.g(), 255 - fill.b())
    }
}

/// The live connection state for one configured remote.
pub struct RemoteLink {
    remote: Remote,
    tunnel: Option<Tunnel>,
    probe: Option<Receiver<Result<String, String>>>,
    next_probe: Option<Instant>,
    /// Daemon version from the last successful probe.
    version: Option<String>,
    /// Why the last attempt did not work, for the hover text.
    error: Option<String>,
    /// Whether the user has asked to be connected. Cleared by disconnect, so
    /// a remote that is simply reachable does not light up on its own.
    wanted: bool,
}

impl RemoteLink {
    pub fn new(remote: Remote) -> Self {
        Self {
            remote,
            tunnel: None,
            probe: None,
            next_probe: None,
            version: None,
            error: None,
            wanted: false,
        }
    }

    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    pub fn light(&self) -> LinkLight {
        if self.version.is_some() {
            LinkLight::Connected
        } else if self.wanted {
            LinkLight::Trying
        } else {
            LinkLight::Blank
        }
    }

    /// Start connecting: bring the tunnel up if the remote needs one, then
    /// let the probe decide when the light turns green.
    pub fn connect(&mut self) {
        self.wanted = true;
        self.error = None;
        if self.tunnel.is_none()
            && let Some(command) = self.remote.tunnel.clone()
        {
            match Tunnel::spawn(&command) {
                Ok(tunnel) => self.tunnel = Some(tunnel),
                Err(err) => {
                    self.error = Some(err);
                    // Probe anyway: a tunnel may already be running from a
                    // terminal, in which case the endpoint answers and the
                    // failed spawn does not matter.
                }
            }
        }
        self.next_probe = Some(Instant::now());
    }

    /// Stop: kill the tunnel and go dark. The sessions already open keep
    /// their own connections — those are the user's to close.
    pub fn disconnect(&mut self) {
        self.wanted = false;
        self.version = None;
        self.error = None;
        self.probe = None;
        self.next_probe = None;
        if let Some(mut tunnel) = self.tunnel.take() {
            tunnel.stop();
        }
    }

    /// Advance the state machine: collect a finished probe, notice a tunnel
    /// that died, and start the next probe when it is due. Call once a frame;
    /// `wake` is invoked when a probe lands so the UI repaints.
    pub fn poll<W>(&mut self, wake: W)
    where
        W: Fn() + Send + 'static,
    {
        if let Some(rx) = &self.probe {
            match rx.try_recv() {
                Ok(Ok(version)) => {
                    self.version = Some(version);
                    self.error = None;
                    self.probe = None;
                    self.next_probe = Some(Instant::now() + PROBE_INTERVAL_UP);
                }
                Ok(Err(err)) => {
                    self.version = None;
                    self.error = Some(err);
                    self.probe = None;
                    self.next_probe = Some(Instant::now() + PROBE_INTERVAL_TRYING);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.probe = None;
                    self.next_probe = Some(Instant::now() + PROBE_INTERVAL_TRYING);
                }
            }
        }

        // A tunnel that exits takes the connection with it: `ssh -N` only
        // ever ends by failing, so its last words are the diagnosis.
        if let Some(tunnel) = &mut self.tunnel
            && let TunnelState::Exited(status) = tunnel.state()
        {
            let tail = tunnel.stderr_tail();
            self.tunnel = None;
            self.version = None;
            self.wanted = false;
            self.next_probe = None;
            let reason = if tail.is_empty() {
                format!("tunnel exited ({status})")
            } else {
                format!("tunnel exited ({status}): {}", tail.join(" · "))
            };
            self.error = Some(reason);
        }

        if !self.wanted || self.probe.is_some() {
            return;
        }
        let due = self.next_probe.is_some_and(|at| Instant::now() >= at);
        if due {
            self.next_probe = None;
            self.probe = Some(spawn_probe(self.remote.endpoint.clone(), wake));
        }
    }

    /// Whether the UI should keep repainting: a pending probe or a scheduled
    /// one means the light can change with no input from the user.
    pub fn wants_repaint(&self) -> bool {
        self.wanted || self.probe.is_some()
    }

    /// Hover text: what this button will do, and why it is not green.
    pub fn hover_text(&self) -> String {
        let name = &self.remote.name;
        let endpoint = &self.remote.endpoint;
        match self.light() {
            LinkLight::Connected => {
                let version = self.version.as_deref().unwrap_or("unknown");
                format!(
                    "connected to {name} ({endpoint}, daemon {version})\n\
                     click: new remote session · right-click: disconnect"
                )
            }
            LinkLight::Trying => {
                let why = self
                    .error
                    .clone()
                    .unwrap_or_else(|| "waiting for the daemon to answer".to_string());
                format!(
                    "connecting to {name} ({endpoint})\n{why}\n\
                     click: retry now · right-click: stop"
                )
            }
            LinkLight::Blank => match &self.error {
                Some(err) => format!("{name} ({endpoint}) disconnected\n{err}\nclick: connect"),
                None => format!("click: connect to {name} ({endpoint})"),
            },
        }
    }
}

/// Blank margin between the button's square and the chain glyph.
const LINK_MARGIN: f32 = 2.0;

/// The connect button: the state color fills the whole square and a chain
/// glyph is inked on top of it. Painted rather than a font glyph so the ink
/// can follow the fill's luminance and the chain can pull apart with the
/// state — a font gives neither, and a ● at this size reads as an
/// off-center dot.
pub fn link_button(bar: BarStyle, ui: &mut Ui, light: LinkLight, colors: &Colors) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(bar.height), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    // Hover and press keep egui's own fills, so the button still feels like
    // the ones beside it; the state color owns the button at rest.
    let fill = if response.hovered() {
        visuals.weak_bg_fill
    } else {
        light.fill(colors, ui.visuals().weak_text_color())
    };
    ui.painter().rect(
        rect,
        visuals.corner_radius,
        fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );
    paint_chain(ui.painter(), rect, light, light.ink(fill));
    response
}

/// The chain: two links laid on the diagonal. The gap between them *is* the
/// state — interlocked when connected, just engaging while trying, pulled
/// apart when there is nothing to connect to.
fn paint_chain(painter: &egui::Painter, rect: Rect, light: LinkLight, ink: Color32) {
    let rect = rect.shrink(LINK_MARGIN);
    let side = rect.width().min(rect.height());
    let (half_len, half_wid, thickness) = link_geometry(side);
    let stroke = Stroke::new(thickness, ink);
    let diagonal = Rot2::from_angle(-std::f32::consts::FRAC_PI_4);
    let gap = link_gap(light, side);
    for offset in [-gap / 2.0, gap / 2.0] {
        let center = rect.center() + diagonal * Vec2::new(offset, 0.0);
        painter.add(Shape::convex_polygon(
            capsule(center, diagonal, half_len, half_wid),
            Color32::TRANSPARENT,
            stroke,
        ));
    }
}

/// One link of the chain: a capsule outline with its long axis along `axis`,
/// as the polygon egui strokes. Sampled point by point because egui's
/// `rect_stroke` only draws axis-aligned rectangles.
fn capsule(center: Pos2, axis: Rot2, half_len: f32, half_wid: f32) -> Vec<Pos2> {
    const STEPS: usize = 12;
    (0..STEPS)
        .map(|step| {
            let phi = step as f32 / STEPS as f32 * std::f32::consts::TAU;
            let (sin, cos) = phi.sin_cos();
            // The sign swings the arc between the two ends of the long
            // axis; the two jumps it makes are the capsule's straight sides.
            let local = Vec2::new(half_len * cos.signum() + half_wid * cos, half_wid * sin);
            center + axis * local
        })
        .collect()
}

/// Link proportions as fractions of the painting side: elongated enough to
/// read as a link, thin enough that the ring's hole survives at bar size.
fn link_geometry(side: f32) -> (f32, f32, f32) {
    (0.17 * side, 0.13 * side, 0.08 * side)
}

/// Center-to-center distance between the two links. Interlocked means the
/// tips cross (`gap < one link's length`); pulled apart leaves daylight.
fn link_gap(light: LinkLight, side: f32) -> f32 {
    match light {
        LinkLight::Connected => 0.30 * side,
        LinkLight::Trying => 0.48 * side,
        LinkLight::Blank => 0.78 * side,
    }
}

/// Probe on a background thread; the UI thread never blocks on a socket.
fn spawn_probe<W>(endpoint: String, wake: W) -> Receiver<Result<String, String>>
where
    W: Fn() + Send + 'static,
{
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("remote-probe".into())
        .spawn(move || {
            let _ = tx.send(bridge::probe(&endpoint, PROBE_TIMEOUT));
            wake();
        })
        .expect("spawn remote-probe thread");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_without_tunnel() -> Remote {
        // Port 0 is never listening: probes fail fast and deterministically.
        Remote {
            name: "test".into(),
            endpoint: "127.0.0.1:1".into(),
            tunnel: None,
            default: true,
        }
    }

    #[test]
    fn starts_blank_and_only_lights_up_when_asked() {
        let mut link = RemoteLink::new(remote_without_tunnel());
        assert_eq!(link.light(), LinkLight::Blank);
        assert!(!link.wants_repaint());
        // Polling an idle link must not start probing on its own.
        link.poll(|| {});
        assert_eq!(link.light(), LinkLight::Blank);
        assert!(link.probe.is_none());
    }

    #[test]
    fn connect_goes_yellow_and_a_failing_probe_keeps_it_there() {
        let mut link = RemoteLink::new(remote_without_tunnel());
        link.connect();
        assert_eq!(link.light(), LinkLight::Trying);
        assert!(link.wants_repaint());

        // Drive it until the probe against a dead port comes back.
        for _ in 0..200 {
            link.poll(|| {});
            if link.error.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(link.light(), LinkLight::Trying, "still trying, not green");
        assert!(link.error.is_some(), "the failure is kept for the hover");
        assert!(link.hover_text().contains("connecting to test"));
    }

    #[test]
    fn disconnect_goes_dark() {
        let mut link = RemoteLink::new(remote_without_tunnel());
        link.connect();
        link.disconnect();
        assert_eq!(link.light(), LinkLight::Blank);
        assert!(!link.wants_repaint());
        assert!(link.hover_text().starts_with("click: connect"));
    }

    #[test]
    fn a_tunnel_that_cannot_start_is_reported_but_still_probes() {
        let mut link = RemoteLink::new(Remote {
            tunnel: Some("kimi-no-such-tunnel-program".into()),
            ..remote_without_tunnel()
        });
        link.connect();
        assert_eq!(link.light(), LinkLight::Trying);
        assert!(
            link.error.as_deref().unwrap().contains("failed to run"),
            "{:?}",
            link.error
        );
        assert!(link.next_probe.is_some(), "a running tunnel may exist");
    }

    #[test]
    fn fill_maps_states_to_status_colors() {
        let colors = crate::theme::Theme::Dark.colors();
        let weak = Color32::from_rgb(1, 2, 3);
        assert_eq!(LinkLight::Connected.fill(&colors, weak), colors.success);
        assert_eq!(LinkLight::Trying.fill(&colors, weak), colors.warning);
        assert_eq!(LinkLight::Blank.fill(&colors, weak), weak);
    }

    #[test]
    fn ink_inverts_its_fill_pairwise() {
        for fill in [
            Color32::from_rgb(10, 20, 30),
            Color32::from_rgb(200, 210, 220),
        ] {
            let ink = LinkLight::Blank.ink(fill);
            // Channels are pairwise complements: light ink on a dark fill
            // and dark ink on a light one, without naming either.
            assert_eq!(ink.r() as u16 + fill.r() as u16, 255);
            assert_eq!(ink.g() as u16 + fill.g() as u16, 255);
            assert_eq!(ink.b() as u16 + fill.b() as u16, 255);
        }
    }

    /// The icon must not need its hue to be read: linked, engaging, apart.
    #[test]
    fn the_gap_tells_the_states_apart() {
        let side = 20.0;
        let (half_len, half_wid, _) = link_geometry(side);
        let length = 2.0 * (half_len + half_wid);
        for light in [LinkLight::Blank, LinkLight::Trying, LinkLight::Connected] {
            let gap = link_gap(light, side);
            let overlap = length - gap;
            match light {
                LinkLight::Connected => assert!(overlap > 0.2 * side),
                LinkLight::Trying => assert!(overlap > 0.0 && overlap < 0.2 * side),
                LinkLight::Blank => assert!(overlap < 0.0, "apart leaves daylight"),
            }
        }
    }

    /// A chain that overflows its square would paint over the strip: the
    /// pulled-apart extent (gap plus one link plus the stroke) has to fit
    /// the *button's* diagonal, the widest thing it can travel along.
    #[test]
    fn the_chain_fits_its_button_in_every_state() {
        let button = 24.0; // the session bar's height
        let side = button - 2.0 * LINK_MARGIN;
        let (half_len, half_wid, thickness) = link_geometry(side);
        for light in [LinkLight::Blank, LinkLight::Trying, LinkLight::Connected] {
            let extent = link_gap(light, side) + 2.0 * (half_len + half_wid) + thickness;
            assert!(
                extent <= std::f32::consts::SQRT_2 * button,
                "{light:?}: chain of {extent} leaves the {button} button"
            );
        }
    }

    /// Painted with `allocate_exact_size`, so this is the guarantee the old
    /// `BarStyle::square` gave its neighbours: a square like theirs.
    #[test]
    fn the_connect_button_is_square_like_its_neighbours() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0))),
            ..Default::default()
        };
        // Fonts are built lazily; the first pass measures against a fallback.
        let _ = ctx.run(input.clone(), |_| {});
        let bar = crate::theme::SESSION_BAR;
        let mut painted: Option<Rect> = None;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                bar.apply(ui);
                painted = Some(
                    link_button(
                        bar,
                        ui,
                        LinkLight::Connected,
                        &crate::theme::Theme::Dark.colors(),
                    )
                    .rect,
                );
            });
        });
        let rect = painted.expect("the button was laid out");
        assert_eq!(
            (rect.width(), rect.height()),
            (bar.height, bar.height),
            "the connect button must sit in the bar's row"
        );
    }
}
