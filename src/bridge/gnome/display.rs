//! Changing mutter's monitor configuration (`-dynres`) and waking its outputs
//! from power save (`-pwrmgr`), both through `org.gnome.Mutter.DisplayConfig`.
//!
//! A resolution change is an `ApplyMonitorsConfig` with method 1 (temporary:
//! applied at once, never saved to monitors.xml, no "Keep changes?" prompt),
//! picking one of the monitor's own modes. mutter cannot be given a new mode
//! over D-Bus: a physical monitor has the modes its EDID lists, a
//! `--virtual-monitor` exactly one, and a `RecordVirtual` monitor either the
//! modes its creator listed or (without a list) whatever size the creator's
//! PipeWire stream negotiates, which only that creator controls.
//!
//! mutter validates the whole layout (no overlaps, every logical monitor
//! adjacent to the rest, one primary, origin at 0,0, a scale the mode
//! supports), so changing one monitor's size moves the monitors that sat to
//! its right or below it by the same amount, which keeps rows and columns
//! intact. Anything mutter still refuses comes back as its own message.
//!
//! Every monitor keeps the properties the user gave it (underscanning, colour
//! mode such as HDR, RGB range): mutter resets whatever a configuration leaves
//! out, so they are read with the layout and passed back with it.
//!
//! Nothing undoes a temporary configuration on its own: the backend keeps the
//! configuration it replaced and applies it again once the last X client
//! stops watching, unless the monitors changed some other way in between
//! (the local user, a hotplug), which then wins.

use std::collections::HashMap;
use std::time::Duration;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

use super::dbus::{DC, LogicalInfo, MonitorInfo, flag};

const DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
/// `ApplyMonitorsConfig` methods.
const METHOD_TEMPORARY: u32 = 1;
/// `layout-mode` values.
const LAYOUT_LOGICAL: u32 = 1;
const LAYOUT_PHYSICAL: u32 = 2;

/// One of a monitor's modes.
#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    pub preferred_scale: f64,
    pub scales: Vec<f64>,
    pub current: bool,
    pub preferred: bool,
    /// A variable refresh rate (VRR) mode; mutter 50 lists one beside each
    /// fixed-rate mode a VRR monitor has.
    pub variable: bool,
}

/// What `ApplyMonitorsConfig` takes per monitor besides its mode. mutter sets
/// anything left out to its default (underscan off, the default colour mode,
/// an automatic RGB range), so a change must carry the current values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MonitorProps {
    /// Only for monitors that support underscanning.
    pub underscanning: Option<bool>,
    pub color_mode: Option<u32>,
    pub rgb_range: Option<u32>,
}

impl MonitorProps {
    fn from_dbus(p: &HashMap<String, OwnedValue>) -> MonitorProps {
        let u32_of = |k: &str| p.get(k).and_then(|v| u32::try_from(v).ok());
        MonitorProps {
            underscanning: p
                .get("is-underscanning")
                .and_then(|v| bool::try_from(v).ok()),
            color_mode: u32_of("color-mode"),
            rgb_range: u32_of("rgb-range"),
        }
    }

    fn to_dbus(&self) -> HashMap<&'static str, Value<'static>> {
        let mut m = HashMap::new();
        if let Some(u) = self.underscanning {
            m.insert("underscanning", Value::Bool(u));
        }
        if let Some(c) = self.color_mode {
            m.insert("color-mode", Value::U32(c));
        }
        if let Some(r) = self.rgb_range {
            m.insert("rgb-range", Value::U32(r));
        }
        m
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorModes {
    /// (connector, vendor, product, serial)
    pub spec: (String, String, String, String),
    pub modes: Vec<Mode>,
    pub for_lease: bool,
    pub props: MonitorProps,
}

impl MonitorModes {
    fn connector(&self) -> &str {
        &self.spec.0
    }

    fn current(&self) -> Option<&Mode> {
        self.modes.iter().find(|m| m.current)
    }
}

/// One logical monitor as `ApplyMonitorsConfig` takes it: position, scale,
/// transform, primary, and each (connector, mode id) shown in it (more than
/// one when mirrored), with that monitor's properties (same order).
#[derive(Clone, Debug)]
pub struct Logical {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub monitors: Vec<(String, String)>,
    pub props: Vec<MonitorProps>,
}

impl PartialEq for Logical {
    fn eq(&self, o: &Self) -> bool {
        // the scale goes through mutter's float, so compare as one
        self.x == o.x
            && self.y == o.y
            && (self.scale as f32 - o.scale as f32).abs() < 1e-4
            && self.transform == o.transform
            && self.primary == o.primary
            && self.monitors == o.monitors
            && self.props == o.props
    }
}

/// A layout in `ApplyMonitorsConfig` terms, in a canonical order so two
/// readings of the same layout compare equal.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout(pub Vec<Logical>);

impl Layout {
    fn new(mut v: Vec<Logical>) -> Layout {
        v.sort_by(|a, b| a.monitors.first().cmp(&b.monitors.first()));
        Layout(v)
    }

    /// For logs: `Meta-0 1280x720@60.000 at 0,0; ...`.
    pub fn describe(&self) -> String {
        self.0
            .iter()
            .map(|l| {
                let mons: Vec<String> =
                    l.monitors.iter().map(|(c, m)| format!("{c} {m}")).collect();
                format!("{} at {},{}", mons.join(" + "), l.x, l.y)
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// mutter's monitor configuration as `GetCurrentState` reports it.
#[derive(Clone, Debug)]
pub struct State {
    pub serial: u32,
    pub monitors: Vec<MonitorModes>,
    pub layout: Layout,
    pub layout_mode: u32,
    /// Whether `layout-mode` may be passed (otherwise mutter uses its default,
    /// which is the current one anyway).
    pub layout_mode_settable: bool,
}

/// What a size request comes to.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// The monitor already runs a mode of that size.
    Unchanged,
    Apply(Layout),
}

impl State {
    pub fn read(conn: &Connection) -> zbus::Result<State> {
        let dc = Proxy::new(conn, DC, DC_PATH, DC)?;
        let (serial, monitors, logical, props): (
            u32,
            Vec<MonitorInfo>,
            Vec<LogicalInfo>,
            HashMap<String, OwnedValue>,
        ) = dc.call("GetCurrentState", &())?;
        Ok(Self::from_parts(serial, monitors, logical, &props))
    }

    fn from_parts(
        serial: u32,
        monitors: Vec<MonitorInfo>,
        logical: Vec<LogicalInfo>,
        props: &HashMap<String, OwnedValue>,
    ) -> State {
        let monitors: Vec<MonitorModes> = monitors
            .into_iter()
            .map(|(spec, modes, mprops)| MonitorModes {
                spec,
                for_lease: flag(&mprops, "is-for-lease"),
                props: MonitorProps::from_dbus(&mprops),
                modes: modes
                    .into_iter()
                    .map(
                        |(id, width, height, refresh, preferred_scale, scales, p)| Mode {
                            id,
                            width,
                            height,
                            refresh,
                            preferred_scale,
                            scales,
                            current: flag(&p, "is-current"),
                            preferred: flag(&p, "is-preferred"),
                            variable: p
                                .get("refresh-rate-mode")
                                .and_then(|v| <&str>::try_from(v).ok())
                                .is_some_and(|m| m == "variable"),
                        },
                    )
                    .collect(),
            })
            .collect();
        let layout = Layout::new(
            logical
                .into_iter()
                .map(|(x, y, scale, transform, primary, mons, _)| {
                    let (monitors, props) = mons
                        .into_iter()
                        .map(|(c, ..)| {
                            let mon = monitors.iter().find(|m| m.connector() == c);
                            let mode = mon
                                .and_then(MonitorModes::current)
                                .map(|m| m.id.clone())
                                .unwrap_or_default();
                            let props = mon.map(|m| m.props.clone()).unwrap_or_default();
                            ((c, mode), props)
                        })
                        .unzip();
                    Logical {
                        x,
                        y,
                        scale,
                        transform,
                        primary,
                        monitors,
                        props,
                    }
                })
                .collect(),
        );
        State {
            serial,
            monitors,
            layout,
            layout_mode: props
                .get("layout-mode")
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(LAYOUT_LOGICAL),
            layout_mode_settable: flag(props, "supports-changing-layout-mode"),
        }
    }

    fn monitor(&self, connector: &str) -> Option<&MonitorModes> {
        self.monitors.iter().find(|m| m.connector() == connector)
    }

    fn mode(&self, connector: &str, id: &str) -> Option<&Mode> {
        self.monitor(connector)?.modes.iter().find(|m| m.id == id)
    }

    /// The logical size of a mode shown at `scale` with `transform`, as mutter
    /// derives it (`derive_logical_monitor_size`).
    fn logical_size(&self, mode: &Mode, scale: f64, transform: u32) -> (i32, i32) {
        let (w, h) = if transform % 2 == 1 {
            (mode.height, mode.width)
        } else {
            (mode.width, mode.height)
        };
        if self.layout_mode == LAYOUT_PHYSICAL {
            return (w, h);
        }
        let s = if scale > 0.0 { scale as f32 } else { 1.0 };
        ((w as f32 / s).round() as i32, (h as f32 / s).round() as i32)
    }

    /// The layout that shows `connector` at `width`x`height` (screen pixels,
    /// i.e. after its rotation), using one of its own modes.
    pub fn plan(&self, connector: &str, width: i32, height: i32) -> Result<Plan, String> {
        let Some(li) = self
            .layout
            .0
            .iter()
            .position(|l| l.monitors.iter().any(|(c, _)| c == connector))
        else {
            return Err(format!("monitor {connector} is not enabled"));
        };
        let old = &self.layout.0[li];
        // the mode is the panel's own orientation
        let (mw, mh) = if old.transform % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        };
        // every monitor of a mirrored group changes together
        let mut chosen = Vec::new();
        for (c, cur_id) in &old.monitors {
            let mon = self
                .monitor(c)
                .ok_or_else(|| format!("monitor {c} vanished"))?;
            let cur = mon.modes.iter().find(|m| &m.id == cur_id);
            let pick = if cur.is_some_and(|m| (m.width, m.height) == (mw, mh)) {
                cur
            } else {
                let refresh = cur.map_or(60.0, |m| m.refresh);
                let variable = cur.is_some_and(|m| m.variable);
                mon.modes
                    .iter()
                    .filter(|m| (m.width, m.height) == (mw, mh))
                    .min_by(|a, b| {
                        // fixed or variable (VRR) refresh as now, then the
                        // closest refresh rate, then the preferred mode,
                        // then the faster one
                        (a.variable != variable)
                            .cmp(&(b.variable != variable))
                            .then(
                                (a.refresh - refresh)
                                    .abs()
                                    .total_cmp(&(b.refresh - refresh).abs()),
                            )
                            .then(b.preferred.cmp(&a.preferred))
                            .then(b.refresh.total_cmp(&a.refresh))
                    })
            };
            let Some(pick) = pick else {
                let mut sizes: Vec<String> = Vec::new();
                for m in &mon.modes {
                    let s = format!("{}x{}", m.width, m.height);
                    if !sizes.contains(&s) {
                        sizes.push(s);
                    }
                }
                return Err(format!(
                    "monitor {c} has no {mw}x{mh} mode (it has {})",
                    sizes.join(", ")
                ));
            };
            chosen.push((c.clone(), pick.clone()));
        }
        if chosen
            .iter()
            .zip(&old.monitors)
            .all(|((_, m), (_, cur))| &m.id == cur)
        {
            return Ok(Plan::Unchanged);
        }
        let first = &chosen[0].1;
        // keep the scale if the new mode supports it (mutter checks the exact
        // float, so reuse the listed value); else the mode's preferred one
        let scale = first
            .scales
            .iter()
            .copied()
            .find(|s| (*s as f32 - old.scale as f32).abs() < 1e-4)
            .or_else(|| {
                first
                    .scales
                    .iter()
                    .copied()
                    .find(|s| (*s as f32 - first.preferred_scale as f32).abs() < 1e-4)
            })
            .or_else(|| first.scales.first().copied())
            .unwrap_or(1.0);
        let old_mode = self
            .mode(&old.monitors[0].0, &old.monitors[0].1)
            .ok_or_else(|| format!("monitor {connector} has no current mode"))?;
        let (ow, oh) = self.logical_size(old_mode, old.scale, old.transform);
        let (nw, nh) = self.logical_size(first, scale, old.transform);
        let (dw, dh) = (nw - ow, nh - oh);
        let (right, bottom) = (old.x + ow, old.y + oh);
        let mut out = Vec::with_capacity(self.layout.0.len());
        for (i, l) in self.layout.0.iter().enumerate() {
            let mut l = l.clone();
            if i == li {
                l.scale = scale;
                l.monitors = chosen
                    .iter()
                    .map(|(c, m)| (c.clone(), m.id.clone()))
                    .collect();
            } else {
                // whatever sat to the right of (or below) the old edge moves
                // with it, so rows and columns stay adjacent
                if l.x >= right {
                    l.x += dw;
                }
                if l.y >= bottom {
                    l.y += dh;
                }
            }
            out.push(l);
        }
        Ok(Plan::Apply(Layout::new(out)))
    }

    /// Whether every mode `layout` names still exists, so applying it can work.
    pub fn can_apply(&self, layout: &Layout) -> bool {
        layout
            .0
            .iter()
            .flat_map(|l| &l.monitors)
            .all(|(c, m)| self.mode(c, m).is_some())
    }
}

/// Applies `layout` as a temporary configuration against `state` (whose
/// serial must still be current).
pub fn apply(conn: &Connection, state: &State, layout: &Layout) -> zbus::Result<()> {
    let dc = Proxy::new(conn, DC, DC_PATH, DC)?;
    type Mon<'a> = (&'a str, &'a str, HashMap<&'static str, Value<'static>>);
    let logical: Vec<(i32, i32, f64, u32, bool, Vec<Mon>)> = layout
        .0
        .iter()
        .map(|l| {
            (
                l.x,
                l.y,
                l.scale,
                l.transform,
                l.primary,
                l.monitors
                    .iter()
                    .enumerate()
                    .map(|(i, (c, m))| {
                        let props = l
                            .props
                            .get(i)
                            .map(MonitorProps::to_dbus)
                            .unwrap_or_default();
                        (c.as_str(), m.as_str(), props)
                    })
                    .collect(),
            )
        })
        .collect();
    let mut props: HashMap<&str, Value> = HashMap::new();
    if state.layout_mode_settable {
        props.insert("layout-mode", Value::U32(state.layout_mode));
    }
    // leased monitors (VR headsets) are not part of the desktop; leaving them
    // out of this list would take them away from whoever leased them
    let leased: Vec<(String, String, String, String)> = state
        .monitors
        .iter()
        .filter(|m| m.for_lease)
        .map(|m| m.spec.clone())
        .collect();
    if !leased.is_empty() {
        props.insert("monitors-for-lease", Value::from(leased));
    }
    dc.call_method(
        "ApplyMonitorsConfig",
        &(state.serial, METHOD_TEMPORARY, logical, props),
    )?;
    Ok(())
}

/// Whether mutter accepts configurations over D-Bus at all (a monitors.xml
/// `<policy>` can turn it off).
pub fn apply_allowed(conn: &Connection) -> bool {
    get_dc_property(conn, "ApplyMonitorsConfigAllowed")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

/// `PowerSaveMode`: 0 on, 1 standby, 2 suspend, 3 off, -1 unsupported.
pub fn power_save_mode(conn: &Connection) -> Option<i32> {
    get_dc_property(conn, "PowerSaveMode").and_then(|v| i32::try_from(v).ok())
}

pub fn set_power_save_mode(conn: &Connection, mode: i32) -> zbus::Result<()> {
    let p = Proxy::new(conn, DC, DC_PATH, "org.freedesktop.DBus.Properties")?;
    p.call_method("Set", &(DC, "PowerSaveMode", Value::I32(mode)))?;
    Ok(())
}

/// How long since the last input event (local or remote), per mutter's core
/// idle monitor.
pub fn idle_time(conn: &Connection) -> Option<Duration> {
    let p = Proxy::new(
        conn,
        "org.gnome.Mutter.IdleMonitor",
        "/org/gnome/Mutter/IdleMonitor/Core",
        "org.gnome.Mutter.IdleMonitor",
    )
    .ok()?;
    p.call::<_, _, u64>("GetIdletime", &())
        .ok()
        .map(Duration::from_millis)
}

/// Reads one property without a caching proxy (whose cache would follow
/// `PropertiesChanged` only as long as the proxy lives).
fn get_dc_property(conn: &Connection, name: &str) -> Option<OwnedValue> {
    let p = Proxy::new(conn, DC, DC_PATH, "org.freedesktop.DBus.Properties").ok()?;
    p.call::<_, _, OwnedValue>("Get", &(DC, name)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(id: &str, w: i32, h: i32, refresh: f64, scales: &[f64], current: bool) -> Mode {
        Mode {
            id: id.into(),
            width: w,
            height: h,
            refresh,
            preferred_scale: 1.0,
            scales: scales.to_vec(),
            current,
            preferred: false,
            variable: false,
        }
    }

    fn monitor(c: &str, modes: Vec<Mode>) -> MonitorModes {
        MonitorModes {
            spec: (c.into(), "v".into(), "p".into(), "s".into()),
            modes,
            for_lease: false,
            props: MonitorProps::default(),
        }
    }

    fn logical(x: i32, y: i32, scale: f64, primary: bool, c: &str, m: &str) -> Logical {
        Logical {
            x,
            y,
            scale,
            transform: 0,
            primary,
            monitors: vec![(c.into(), m.into())],
            props: vec![MonitorProps::default()],
        }
    }

    /// Two monitors side by side: A (1280x720, modes 1024x768, 1920x1080 at 60
    /// and 30) left of B (1024x768).
    fn two() -> State {
        State {
            serial: 7,
            monitors: vec![
                monitor(
                    "A",
                    vec![
                        mode("A720", 1280, 720, 60.0, &[1.0, 1.25], true),
                        mode("A768", 1024, 768, 60.0, &[1.0], false),
                        mode("A1080@30", 1920, 1080, 30.0, &[1.0, 2.0], false),
                        mode("A1080@60", 1920, 1080, 60.0, &[1.0, 2.0], false),
                    ],
                ),
                monitor("B", vec![mode("B768", 1024, 768, 60.0, &[1.0], true)]),
            ],
            layout: Layout::new(vec![
                logical(0, 0, 1.0, true, "A", "A720"),
                logical(1280, 0, 1.0, false, "B", "B768"),
            ]),
            layout_mode: LAYOUT_LOGICAL,
            layout_mode_settable: true,
        }
    }

    #[test]
    fn growing_a_monitor_moves_its_right_neighbour() {
        let Plan::Apply(l) = two().plan("A", 1920, 1080).unwrap() else {
            panic!("expected a change")
        };
        assert_eq!(
            l,
            Layout::new(vec![
                logical(0, 0, 1.0, true, "A", "A1080@60"),
                logical(1920, 0, 1.0, false, "B", "B768"),
            ])
        );
    }

    #[test]
    fn shrinking_the_right_monitor_moves_nothing_else() {
        let mut s = two();
        s.monitors[1]
            .modes
            .push(mode("B600", 800, 600, 60.0, &[1.0], false));
        let Plan::Apply(l) = s.plan("B", 800, 600).unwrap() else {
            panic!("expected a change")
        };
        assert_eq!(
            l,
            Layout::new(vec![
                logical(0, 0, 1.0, true, "A", "A720"),
                logical(1280, 0, 1.0, false, "B", "B600"),
            ])
        );
    }

    #[test]
    fn the_current_size_is_unchanged() {
        assert_eq!(two().plan("A", 1280, 720), Ok(Plan::Unchanged));
    }

    #[test]
    fn a_size_without_a_mode_is_refused_with_the_sizes_there_are() {
        let e = two().plan("B", 1920, 1080).unwrap_err();
        assert!(
            e.contains("no 1920x1080 mode") && e.contains("1024x768"),
            "{e}"
        );
    }

    #[test]
    fn a_disabled_monitor_is_refused() {
        let mut s = two();
        s.layout.0.retain(|l| l.monitors[0].0 == "A");
        assert!(s.plan("B", 1024, 768).unwrap_err().contains("not enabled"));
    }

    #[test]
    fn an_unsupported_scale_falls_back_to_the_modes_preferred() {
        let mut s = two();
        s.layout.0[0].scale = 1.25;
        s.layout.0[1].x = 1024; // A is 1280/1.25 = 1024 logical pixels wide
        let Plan::Apply(l) = s.plan("A", 1024, 768).unwrap() else {
            panic!("expected a change")
        };
        let a = l.0.iter().find(|l| l.monitors[0].0 == "A").unwrap();
        assert_eq!(a.scale, 1.0);
        // B sat at A's old logical right edge (1280/1.25 = 1024): stays adjacent
        let b = l.0.iter().find(|l| l.monitors[0].0 == "B").unwrap();
        assert_eq!(b.x, 1024);
    }

    #[test]
    fn a_rotated_monitor_takes_the_swapped_mode() {
        let mut s = two();
        s.layout.0[1].transform = 1; // B is portrait: 768x1024 on screen
        s.monitors[1]
            .modes
            .push(mode("B600", 800, 600, 60.0, &[1.0], false));
        let Plan::Apply(l) = s.plan("B", 600, 800).unwrap() else {
            panic!("expected a change")
        };
        let b = l.0.iter().find(|l| l.monitors[0].0 == "B").unwrap();
        assert_eq!(b.monitors[0].1, "B600");
    }

    #[test]
    fn the_closest_refresh_rate_wins() {
        let mut s = two();
        s.monitors[0].modes[0].refresh = 30.0;
        let Plan::Apply(l) = s.plan("A", 1920, 1080).unwrap() else {
            panic!("expected a change")
        };
        let a = l.0.iter().find(|l| l.monitors[0].0 == "A").unwrap();
        assert_eq!(a.monitors[0].1, "A1080@30");
    }

    #[test]
    fn a_monitor_below_moves_down() {
        let mut s = two();
        s.layout.0[1].x = 0;
        s.layout.0[1].y = 720;
        let Plan::Apply(l) = s.plan("A", 1024, 768).unwrap() else {
            panic!("expected a change")
        };
        let b = l.0.iter().find(|l| l.monitors[0].0 == "B").unwrap();
        assert_eq!((b.x, b.y), (0, 768));
    }

    #[test]
    fn layouts_compare_regardless_of_order_and_float_noise() {
        let a = Layout::new(vec![
            logical(1280, 0, 1.0, false, "B", "B768"),
            logical(0, 0, 1.333_333_373_069_763_2, true, "A", "A720"),
        ]);
        let b = Layout::new(vec![
            logical(0, 0, 4.0 / 3.0, true, "A", "A720"),
            logical(1280, 0, 1.0, false, "B", "B768"),
        ]);
        assert_eq!(a, b);
    }

    #[test]
    fn the_refusal_lists_each_size_once() {
        let mut s = two();
        s.monitors[1].modes = vec![
            mode("B768", 1024, 768, 60.0, &[1.0], true),
            mode("B600", 800, 600, 60.0, &[1.0], false),
            mode("B768@50", 1024, 768, 50.0, &[1.0], false),
        ];
        let e = s.plan("B", 1920, 1080).unwrap_err();
        assert!(e.ends_with("(it has 1024x768, 800x600)"), "{e}");
    }

    #[test]
    fn a_fixed_rate_mode_stays_fixed_rate() {
        let mut s = two();
        // VRR first, as mutter may list it
        let mut vrr = mode("A1080vrr", 1920, 1080, 60.0, &[1.0], false);
        vrr.variable = true;
        s.monitors[0].modes.insert(0, vrr);
        let Plan::Apply(l) = s.plan("A", 1920, 1080).unwrap() else {
            panic!("expected a change")
        };
        let a = l.0.iter().find(|l| l.monitors[0].0 == "A").unwrap();
        assert_eq!(a.monitors[0].1, "A1080@60");
        // and a VRR mode picks VRR
        s.monitors[0].modes[1].variable = true; // the current A720
        let Plan::Apply(l) = s.plan("A", 1920, 1080).unwrap() else {
            panic!("expected a change")
        };
        let a = l.0.iter().find(|l| l.monitors[0].0 == "A").unwrap();
        assert_eq!(a.monitors[0].1, "A1080vrr");
    }

    #[test]
    fn a_change_keeps_every_monitors_properties() {
        let mut s = two();
        let hdr = MonitorProps {
            underscanning: Some(true),
            color_mode: Some(1),
            rgb_range: Some(2),
        };
        s.layout.0[1].props = vec![hdr.clone()];
        let Plan::Apply(l) = s.plan("A", 1920, 1080).unwrap() else {
            panic!("expected a change")
        };
        let b = l.0.iter().find(|l| l.monitors[0].0 == "B").unwrap();
        assert_eq!(b.props, vec![hdr.clone()]);
        let d = b.props[0].to_dbus();
        assert_eq!(d.get("underscanning"), Some(&Value::Bool(true)));
        assert_eq!(d.get("color-mode"), Some(&Value::U32(1)));
        assert_eq!(d.get("rgb-range"), Some(&Value::U32(2)));
        // a property change made meanwhile is a reconfiguration
        let mut changed = l.clone();
        changed.0[1].props[0].color_mode = Some(0);
        assert_ne!(l, changed);
    }

    #[test]
    fn mirrored_monitors_change_together() {
        let mut s = two();
        s.monitors[1]
            .modes
            .push(mode("B1080", 1920, 1080, 60.0, &[1.0, 2.0], false));
        s.monitors[1].modes[0].current = true;
        s.layout = Layout::new(vec![Logical {
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: true,
            monitors: vec![("A".into(), "A720".into()), ("B".into(), "B768".into())],
            props: vec![MonitorProps::default(), MonitorProps::default()],
        }]);
        let Plan::Apply(l) = s.plan("B", 1920, 1080).unwrap() else {
            panic!("expected a change")
        };
        assert_eq!(
            l.0[0].monitors,
            vec![
                ("A".to_string(), "A1080@60".to_string()),
                ("B".to_string(), "B1080".to_string())
            ]
        );
        // a size one of them lacks is refused for the group
        assert!(s.plan("A", 800, 600).is_err());
    }

    #[test]
    fn can_apply_needs_every_mode() {
        let s = two();
        assert!(s.can_apply(&s.layout));
        let gone = Layout::new(vec![logical(0, 0, 1.0, true, "A", "A480")]);
        assert!(!s.can_apply(&gone));
    }
}
