// src/matter/registry.rs
//
// Generic Matter endpoint registry + flat router (Phase 19g.1).
//
// WHY THIS EXISTS. Until this module, every Matter device type was a
// singleton with a fixed endpoint id, chained onto the node's handler with
// rs-matter's `ChainedHandler`, which nests ONE generic layer per cluster.
// That shape could not grow:
//   * one `[matter.<type>]` section per kind meant a board with four relays
//     or six sensors was impossible;
//   * the compile-time type nesting had already forced `#![recursion_limit =
//     "256"]` at just four device types (rustc: "queries overflow the depth
//     limit" — only a full `cargo build` hit it; check/clippy/test did not),
//     and every added cluster deepened it. With this router that attribute
//     is gone;
//   * each new device type edited mod.rs/config.rs in ~6 places.
//
// THE FIX. Handlers live in a `Vec` of `(endpoint, cluster, ClusterImpl)`
// where `ClusterImpl` is an ENUM with one variant per cluster-handler type.
// Dispatch is a hash lookup followed by a `match`, so:
//   * the static type is flat (O(1) nesting, not O(#clusters)) — no more
//     recursion-limit growth;
//   * the NUMBER of endpoints/clusters is a runtime quantity (N instances of
//     any kind, driven by config);
//   * enum dispatch stays statically typed — no `dyn` needed even though
//     `AsyncHandler`'s methods are generic over the (concrete, private)
//     IM context types, which makes the trait non-object-safe.
// The system clusters (root endpoint 0) stay on rs-matter's own chain, which
// the router wraps and falls through to for anything it doesn't own.
//
// Adding a cluster-handler type = one line in the `cluster_impls!` list below
// plus its constructor; nothing else in this file changes.
use core::future::{poll_fn, Future};
use core::pin::Pin;
use core::task::Poll;
use std::collections::{HashMap, HashSet};

use rs_matter::dm::clusters::app::{cam_av_stream, color_control, level_control, on_off, webrtc_prov, zone_mgmt};
use rs_matter::dm::clusters::decl::{
    boolean_state, fan_control, flow_measurement, illuminance_measurement, occupancy_sensing,
    pressure_measurement, relative_humidity_measurement, temperature_measurement,
};
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::identify;
use rs_matter::dm::{
    Async, AsyncHandler, Cluster, ClusterId, Dataver, DeviceType, Endpoint, EndptId,
    HandlerContext, InvokeContext, InvokeReply, LifecycleOp, MatchContext, ReadContext, ReadReply,
    SemanticTag, WriteContext,
};
use rs_matter::error::Error;
use rs_matter::with;

use crate::matter::{actuators, camera, light, onoff, sensors, thermostat};

/// First endpoint id handed out to endpoints that don't pin one. 1-4 are
/// reserved for the legacy singletons (camera/onoff/light/thermostat) so a
/// controller that already paired keeps seeing the same ids.
pub(crate) const FIRST_DYNAMIC_ENDPOINT_ID: EndptId = 16;

macro_rules! cluster_impls {
    ($( $variant:ident($ty:ty) ),+ $(,)?) => {
        /// One variant per concrete cluster-handler type (see module header).
        pub(crate) enum ClusterImpl {
            $( $variant($ty) ),+
        }

        impl AsyncHandler for ClusterImpl {
            fn read_awaits(&self, ctx: impl ReadContext) -> bool {
                match self { $( Self::$variant(h) => h.read_awaits(ctx) ),+ }
            }

            fn write_awaits(&self, ctx: impl WriteContext) -> bool {
                match self { $( Self::$variant(h) => h.write_awaits(ctx) ),+ }
            }

            fn invoke_awaits(&self, ctx: impl InvokeContext) -> bool {
                match self { $( Self::$variant(h) => h.invoke_awaits(ctx) ),+ }
            }

            async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
                match self { $( Self::$variant(h) => h.read(ctx, reply).await ),+ }
            }

            async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
                match self { $( Self::$variant(h) => h.write(ctx).await ),+ }
            }

            async fn invoke(
                &self,
                ctx: impl InvokeContext,
                reply: impl InvokeReply,
            ) -> Result<(), Error> {
                match self { $( Self::$variant(h) => h.invoke(ctx, reply).await ),+ }
            }

            fn bump_dataver(&self, ctx: impl MatchContext) {
                match self { $( Self::$variant(h) => h.bump_dataver(ctx) ),+ }
            }

            fn lifecycle(&self, ctx: impl HandlerContext, op: LifecycleOp) -> Result<(), Error> {
                match self { $( Self::$variant(h) => h.lifecycle(ctx, op) ),+ }
            }

            async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
                match self { $( Self::$variant(h) => h.run(ctx).await ),+ }
            }
        }
    };
}

cluster_impls! {
    Desc(Async<desc::HandlerAdaptor<desc::DescHandler<'static>>>),
    RelayOnOff(on_off::HandlerAsyncAdaptor<&'static onoff::OnOff>),
    LightOnOff(on_off::HandlerAsyncAdaptor<&'static light::LightOnOff>),
    LightLevel(level_control::HandlerAsyncAdaptor<&'static light::LightLevel>),
    LightColor(color_control::HandlerAsyncAdaptor<&'static light::LightColor>),
    Thermostat(Async<&'static thermostat::ThermostatHandler>),
    CamAv(cam_av_stream::HandlerAsyncAdaptor<&'static camera::CamAv>),
    ZoneMgmt(zone_mgmt::HandlerAsyncAdaptor<&'static camera::ZoneMgmt>),
    WebRtc(webrtc_prov::HandlerAsyncAdaptor<&'static camera::WebRtc>),
    Identify(Async<identify::HandlerAdaptor<identify::IdentifyHandler<()>>>),
    Temperature(Async<temperature_measurement::HandlerAdaptor<sensors::TemperatureHandler>>),
    Humidity(Async<relative_humidity_measurement::HandlerAdaptor<sensors::HumidityHandler>>),
    Pressure(Async<pressure_measurement::HandlerAdaptor<sensors::PressureHandler>>),
    Illuminance(Async<illuminance_measurement::HandlerAdaptor<sensors::IlluminanceHandler>>),
    Flow(Async<flow_measurement::HandlerAdaptor<sensors::FlowHandler>>),
    Occupancy(Async<occupancy_sensing::HandlerAdaptor<sensors::OccupancyHandler>>),
    BooleanState(Async<boolean_state::HandlerAdaptor<sensors::BooleanStateHandler>>),
    SinkLight(on_off::HandlerAsyncAdaptor<&'static actuators::LightOnOff>),
    SinkPlug(on_off::HandlerAsyncAdaptor<&'static actuators::PlugOnOff>),
    Fan(Async<fan_control::HandlerAdaptor<actuators::FanHandler>>),
}

/// The Identify cluster, MANDATORY on every sensor and actuator device type
/// (verified against the spec model). No hardware indicator is wired up, so
/// `IdentifyType` is None — still conformant, and it lets a controller's
/// "identify" button succeed.
pub(crate) fn identify_cluster<R: rand_core::Rng>(rand: &mut R) -> (Cluster<'static>, ClusterImpl) {
    (
        identify::CLUSTER,
        ClusterImpl::Identify(Async(
            identify::IdentifyHandler::new(Dataver::new_rand(rand)).adapt(),
        )),
    )
}

/// What a device kind contributes: one Matter endpoint and its clusters.
/// The Descriptor cluster is NOT listed — `Registry::build` adds one per
/// endpoint (it needs the final endpoint id / tag list, which only the
/// registry knows).
pub(crate) struct EndpointSpec {
    /// Final endpoint id, obtained from `Registry::reserve` (pinned) or
    /// `Registry::alloc` (dynamic) BEFORE the handlers were built — a sensor's
    /// background task needs its own endpoint id to notify subscribers.
    pub id: EndptId,
    /// True for config-driven endpoints (auto or pinned in `[[matter.endpoints]]`),
    /// which advertise a stable `UniqueID` derived from `name`. The four legacy
    /// singletons (camera/onoff/light/thermostat) keep their original Descriptor
    /// metadata untouched so already-paired controllers see no change.
    pub dynamic: bool,
    /// Stable human-meaningful name (config `name`, or the legacy kind name),
    /// used for the `UniqueID` and, when needed, the semantic-tag label.
    pub name: String,
    pub device_types: Vec<DeviceType>,
    pub clusters: Vec<(Cluster<'static>, ClusterImpl)>,
}

impl EndpointSpec {
    /// Add the Identify cluster. The spec makes it mandatory on the On/Off Light,
    /// Extended Color Light and Thermostat device types the legacy `[matter.*]`
    /// sections expose (optional on Camera, so that one is left alone). It is
    /// purely additive: the existing clusters, ids and attribute data are
    /// untouched, so an already-paired controller keeps working.
    pub(crate) fn with_identify<R: rand_core::Rng>(mut self, rand: &mut R) -> Self {
        self.clusters.insert(0, identify_cluster(rand));
        self
    }
}

struct Entry {
    endpoint: EndptId,
    cluster: ClusterId,
    imp: ClusterImpl,
}

/// The flat dispatcher. Owns every device-endpoint cluster handler and
/// delegates anything it doesn't own (the root endpoint's system clusters)
/// to `next`.
pub(crate) struct Router<N> {
    entries: Vec<Entry>,
    index: HashMap<(EndptId, ClusterId), usize>,
    next: N,
}

impl<N> Router<N> {
    fn lookup(&self, endpoint: EndptId, cluster: ClusterId) -> Option<usize> {
        self.index.get(&(endpoint, cluster)).copied()
    }
}

impl<N: AsyncHandler> AsyncHandler for Router<N> {
    fn read_awaits(&self, ctx: impl ReadContext) -> bool {
        let idx = {
            let a = ctx.attr();
            self.lookup(a.endpoint_id, a.cluster_id)
        };
        match idx {
            Some(i) => self.entries[i].imp.read_awaits(ctx),
            None => self.next.read_awaits(ctx),
        }
    }

    fn write_awaits(&self, ctx: impl WriteContext) -> bool {
        let idx = {
            let a = ctx.attr();
            self.lookup(a.endpoint_id, a.cluster_id)
        };
        match idx {
            Some(i) => self.entries[i].imp.write_awaits(ctx),
            None => self.next.write_awaits(ctx),
        }
    }

    fn invoke_awaits(&self, ctx: impl InvokeContext) -> bool {
        let idx = {
            let c = ctx.cmd();
            self.lookup(c.endpoint_id, c.cluster_id)
        };
        match idx {
            Some(i) => self.entries[i].imp.invoke_awaits(ctx),
            None => self.next.invoke_awaits(ctx),
        }
    }

    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let idx = {
            let a = ctx.attr();
            self.lookup(a.endpoint_id, a.cluster_id)
        };
        match idx {
            Some(i) => self.entries[i].imp.read(ctx, reply).await,
            None => self.next.read(ctx, reply).await,
        }
    }

    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        let idx = {
            let a = ctx.attr();
            self.lookup(a.endpoint_id, a.cluster_id)
        };
        match idx {
            Some(i) => self.entries[i].imp.write(ctx).await,
            None => self.next.write(ctx).await,
        }
    }

    async fn invoke(&self, ctx: impl InvokeContext, reply: impl InvokeReply) -> Result<(), Error> {
        let idx = {
            let c = ctx.cmd();
            self.lookup(c.endpoint_id, c.cluster_id)
        };
        match idx {
            Some(i) => self.entries[i].imp.invoke(ctx, reply).await,
            None => self.next.invoke(ctx, reply).await,
        }
    }

    fn bump_dataver(&self, ctx: impl MatchContext) {
        // Same semantics as `ChainedHandler`: the owning handler bumps, and
        // the call ALSO falls through to `next` (several handlers may need to
        // bump for one operation). A context with no endpoint/cluster is a
        // "global" bump that every handler must see.
        match (ctx.endpt(), ctx.cluster()) {
            (Some(e), Some(c)) => {
                if let Some(i) = self.lookup(e, c) {
                    self.entries[i].imp.bump_dataver(&ctx);
                }
            }
            _ => {
                for entry in &self.entries {
                    entry.imp.bump_dataver(&ctx);
                }
            }
        }
        self.next.bump_dataver(ctx);
    }

    fn lifecycle(&self, ctx: impl HandlerContext, op: LifecycleOp) -> Result<(), Error> {
        // Lifecycle ops are a broadcast: every handler sees them.
        for entry in &self.entries {
            entry.imp.lifecycle(&ctx, op)?;
        }
        self.next.lifecycle(ctx, op)
    }

    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        // Join every handler's background task (and the system chain's).
        // Most pend forever; if any ends (normally with an error) the whole
        // set ends with that result — the same contract `ChainedHandler`'s
        // `select(...).coalesce()` gave, just over a runtime-sized set.
        type Task<'t> = Pin<Box<dyn Future<Output = Result<(), Error>> + 't>>;
        let mut tasks: Vec<Task<'_>> = Vec::with_capacity(self.entries.len() + 1);
        for entry in &self.entries {
            tasks.push(Box::pin(entry.imp.run(&ctx)));
        }
        tasks.push(Box::pin(self.next.run(&ctx)));

        poll_fn(|cx| {
            for task in tasks.iter_mut() {
                if let Poll::Ready(result) = task.as_mut().poll(cx) {
                    return Poll::Ready(result);
                }
            }
            Poll::Pending
        })
        .await
    }
}

/// Collects endpoint specs; `plan()` then builds the node's endpoint list and
/// `Planned::into_router()` produces the dispatcher.
///
/// Endpoint ids are handed out up front (`reserve` for pinned ids, `alloc` for
/// dynamic ones) so a device's handlers can be built knowing their final id.
/// Callers must `reserve` every pinned id BEFORE the first `alloc`, otherwise a
/// dynamic id could be taken that a later pinned endpoint wanted.
///
/// Two build steps (not one) because the system handler chain
/// (`EthSysHandlerBuilder::build`) consumes the RNG by value, so it can only
/// be built AFTER planning (which needs `&mut rng` for the per-endpoint
/// Descriptor `Dataver`s), and the router wraps that chain.
pub(crate) struct Registry {
    specs: Vec<EndpointSpec>,
    used: HashSet<EndptId>,
    next_dynamic: EndptId,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            specs: Vec::new(),
            used: HashSet::new(),
            next_dynamic: FIRST_DYNAMIC_ENDPOINT_ID,
        }
    }
}

/// The result of `Registry::plan`: the final endpoint list plus every
/// handler, ready to be wrapped around the system chain.
pub(crate) struct Planned {
    /// Root endpoint first, then every device endpoint. Leaked to `'static`:
    /// this runs once at boot and the node lives for the process lifetime.
    pub(crate) endpoints: &'static [Endpoint<'static>],
    entries: Vec<Entry>,
    index: HashMap<(EndptId, ClusterId), usize>,
}

impl Planned {
    pub(crate) fn into_router<N: AsyncHandler>(self, next: N) -> Router<N> {
        Router {
            entries: self.entries,
            index: self.index,
            next,
        }
    }
}

impl Registry {
    /// Claim a specific endpoint id (a legacy singleton's fixed id, or a
    /// config-pinned one). 0 is the root endpoint; ids are unique.
    pub(crate) fn reserve(&mut self, id: EndptId) -> Result<EndptId, String> {
        if id == 0 {
            return Err("endpoint id 0 is reserved for the root endpoint".to_string());
        }
        if !self.used.insert(id) {
            return Err(format!("endpoint id {id} is claimed by more than one endpoint"));
        }
        Ok(id)
    }

    /// Allocate the next free dynamic id (from `FIRST_DYNAMIC_ENDPOINT_ID`).
    pub(crate) fn alloc(&mut self) -> Result<EndptId, String> {
        while self.used.contains(&self.next_dynamic) {
            self.next_dynamic = self
                .next_dynamic
                .checked_add(1)
                .ok_or_else(|| "ran out of endpoint ids".to_string())?;
        }
        let id = self.next_dynamic;
        self.used.insert(id);
        Ok(id)
    }

    pub(crate) fn add(&mut self, spec: EndpointSpec) {
        self.specs.push(spec);
    }

    /// Assign ids, add a Descriptor cluster per endpoint, and build the
    /// runtime endpoint list (root endpoint first).
    pub(crate) fn plan<R>(
        self,
        rand: &mut R,
        root: Endpoint<'static>,
        vendor_id: u16,
    ) -> Result<Planned, String>
    where
        R: rand_core::Rng,
    {
        let primary_types: Vec<u16> = self
            .specs
            .iter()
            .map(|s| s.device_types.first().map_or(0, |d| d.dtype))
            .collect();
        let tag_slots = tag_slots(&primary_types)?;
        for spec in &self.specs {
            if spec.name.len() > MAX_NAME_BYTES {
                return Err(format!(
                    "endpoint name '{}' is longer than {MAX_NAME_BYTES} bytes",
                    spec.name
                ));
            }
        }

        let mut endpoints: Vec<Endpoint<'static>> = vec![root];
        let mut entries: Vec<Entry> = Vec::new();

        for (spec, tag_slot) in self.specs.into_iter().zip(tag_slots) {
            let id = spec.id;
            // Dynamic (auto-id) endpoints advertise a stable UniqueID derived
            // from their name, so integrations can keep addressing "the same"
            // endpoint if ids are ever renumbered. Pinned (legacy) endpoints
            // keep their original Descriptor metadata untouched.
            let unique_id: Option<&'static str> = if spec.dynamic {
                Some(Box::leak(spec.name.clone().into_boxed_str()))
            } else {
                None
            };
            let semantic_tags: &'static [SemanticTag<'static>] = match tag_slot {
                Some(n) => {
                    let label: &'static str = Box::leak(spec.name.clone().into_boxed_str());
                    Box::leak(
                        vec![SemanticTag {
                            mfg_code: Some(vendor_id),
                            namespace_id: 0,
                            tag: n,
                            label: Some(label),
                        }]
                        .into_boxed_slice(),
                    )
                }
                None => &[],
            };
            let desc_meta = descriptor_cluster(tag_slot.is_some(), unique_id.is_some());

            let mut metas: Vec<Cluster<'static>> = vec![desc_meta.clone()];
            entries.push(Entry {
                endpoint: id,
                cluster: desc::DescHandler::CLUSTER.id,
                imp: ClusterImpl::Desc(Async(
                    desc::DescHandler::new(Dataver::new_rand(rand)).adapt(),
                )),
            });
            for (meta, imp) in spec.clusters {
                entries.push(Entry {
                    endpoint: id,
                    cluster: meta.id,
                    imp,
                });
                metas.push(meta);
            }

            let device_types: &'static [DeviceType] =
                Box::leak(spec.device_types.into_boxed_slice());
            let clusters: &'static [Cluster<'static>] = Box::leak(metas.into_boxed_slice());
            endpoints.push(Endpoint {
                unique_id,
                semantic_tags,
                ..Endpoint::new(id, device_types, clusters)
            });
        }

        let mut index = HashMap::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            if index.insert((entry.endpoint, entry.cluster), i).is_some() {
                return Err(format!(
                    "duplicate handler for endpoint {} cluster 0x{:04X}",
                    entry.endpoint, entry.cluster
                ));
            }
        }

        Ok(Planned {
            endpoints: Box::leak(endpoints.into_boxed_slice()),
            entries,
            index,
        })
    }
}

/// Longest endpoint name we accept: it becomes the endpoint's `UniqueID`
/// (max 32 bytes in the spec) and a semantic-tag label (max 64).
pub(crate) const MAX_NAME_BYTES: usize = 32;

/// Descriptor cluster metadata for one endpoint, advertising the optional
/// `TagList` / `EndpointUniqueID` attributes ONLY when the endpoint actually
/// carries them — advertising an attribute with no value would turn every
/// read of it into an error (rs-matter's own docs say the same).
fn descriptor_cluster(tags: bool, unique_id: bool) -> Cluster<'static> {
    match (tags, unique_id) {
        (false, false) => desc::DescHandler::CLUSTER,
        (true, false) => desc::CLUSTER_TAG_LIST,
        (false, true) => desc::CLUSTER_ENDPOINT_UNIQUE_ID,
        (true, true) => desc::FULL_CLUSTER
            .with_attrs(with!(required; desc::AttributeId::TagList | desc::AttributeId::EndpointUniqueID))
            .with_cmds(with!())
            .with_features(desc::Feature::TAG_LIST.bits()),
    }
}

/// Matter Core spec 9.5: when two or more endpoints on a node share a device
/// type, each of them must carry a non-empty `TagList`, and no two lists may be
/// identical (certification test `TC_DESC_2_2`). This returns, per endpoint
/// (by its primary device type), `None` when its type is unique on the node,
/// or `Some(n)` — a distinct 1-based occurrence number to use as the tag —
/// when the type is shared. A single On/Off Light therefore stays untagged
/// (existing paired controllers see no change), while four relays get tags
/// 1..=4.
pub(crate) fn tag_slots(primary_types: &[u16]) -> Result<Vec<Option<u8>>, String> {
    let mut counts: HashMap<u16, usize> = HashMap::new();
    for t in primary_types {
        *counts.entry(*t).or_default() += 1;
    }
    let mut seen: HashMap<u16, u8> = HashMap::new();
    let mut out = Vec::with_capacity(primary_types.len());
    for t in primary_types {
        if counts[t] < 2 {
            out.push(None);
            continue;
        }
        let n = seen.entry(*t).or_default();
        *n = n
            .checked_add(1)
            .ok_or_else(|| format!("more than 255 endpoints share device type 0x{t:04X}"))?;
        out.push(Some(*n));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_starts_at_16_in_order() {
        let mut r = Registry::default();
        assert_eq!([r.alloc().unwrap(), r.alloc().unwrap(), r.alloc().unwrap()], [16, 17, 18]);
    }

    #[test]
    fn alloc_skips_reserved_ids() {
        let mut r = Registry::default();
        r.reserve(2).unwrap();
        r.reserve(16).unwrap();
        r.reserve(17).unwrap();
        assert_eq!(r.alloc().unwrap(), 18);
        assert_eq!(r.alloc().unwrap(), 19);
    }

    #[test]
    fn legacy_ids_do_not_collide_with_dynamic_ones() {
        let mut r = Registry::default();
        for id in 1..=4 {
            r.reserve(id).unwrap();
        }
        assert_eq!(r.alloc().unwrap(), 16);
    }

    #[test]
    fn duplicate_reservations_are_rejected() {
        let mut r = Registry::default();
        r.reserve(5).unwrap();
        assert!(r.reserve(5).is_err());
    }

    #[test]
    fn alloc_never_reuses_an_id() {
        let mut r = Registry::default();
        let a = r.alloc().unwrap();
        assert!(r.reserve(a).is_err(), "an allocated id is claimed");
    }

    #[test]
    fn root_id_is_rejected() {
        assert!(Registry::default().reserve(0).is_err());
    }

    #[test]
    fn unique_device_types_get_no_tags() {
        assert_eq!(
            tag_slots(&[0x0142, 0x0100, 0x010D, 0x0301]).unwrap(),
            vec![None, None, None, None]
        );
    }

    #[test]
    fn shared_device_types_get_distinct_tags_and_unique_ones_stay_untagged() {
        // three on/off lights (0x0100) and one thermostat (0x0301)
        assert_eq!(
            tag_slots(&[0x0100, 0x0301, 0x0100, 0x0100]).unwrap(),
            vec![Some(1), None, Some(2), Some(3)]
        );
    }

    #[test]
    fn tags_count_per_device_type() {
        assert_eq!(
            tag_slots(&[0x0100, 0x0302, 0x0100, 0x0302]).unwrap(),
            vec![Some(1), Some(1), Some(2), Some(2)]
        );
    }

    #[test]
    fn descriptor_metadata_advertises_only_what_the_endpoint_carries() {
        let has = |c: &Cluster<'static>, attr: u32| c.attribute(attr).is_some();
        let plain = descriptor_cluster(false, false);
        assert!(!has(&plain, 4) && !has(&plain, 5));
        let tags = descriptor_cluster(true, false);
        assert!(has(&tags, 4) && !has(&tags, 5));
        let uid = descriptor_cluster(false, true);
        assert!(!has(&uid, 4) && has(&uid, 5));
        let both = descriptor_cluster(true, true);
        assert!(has(&both, 4) && has(&both, 5));
        assert_eq!(both.feature_map, 1, "TAG_LIST feature bit");
    }
}
