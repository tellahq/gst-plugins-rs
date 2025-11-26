use ges::prelude::*;
use gst::glib::translate::*;
use gst::glib::{self, subclass::prelude::*, SignalHandlerId, WeakRef};
use gst::subclass::prelude::*;
use gst_controller::prelude::*;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "gessubtimelineprimarymanager",
        gst::DebugColorFlags::FG_YELLOW | gst::DebugColorFlags::BOLD,
        Some("GES Subtimeline Primary Manager"),
    )
});

unsafe extern "C" {
    fn ges_timeline_acquire(timeline: *mut ges::ffi::GESTimeline);
    fn ges_timeline_release(timeline: *mut ges::ffi::GESTimeline);
}

struct TimelineLockGuard {
    timeline: ges::Timeline,
}

impl TimelineLockGuard {
    fn new(timeline: &ges::Timeline) -> Self {
        unsafe {
            ges_timeline_acquire(timeline.as_ptr() as *mut ges::ffi::GESTimeline);
        }
        Self {
            timeline: timeline.clone(),
        }
    }
}

impl Drop for TimelineLockGuard {
    fn drop(&mut self) {
        unsafe {
            ges_timeline_release(self.timeline.as_ptr() as *mut ges::ffi::GESTimeline);
        }
    }
}

#[derive(Debug, Clone)]
struct Replica {
    id: u64,
    timeline: WeakRef<ges::Timeline>,
    element_mappings: Arc<Mutex<HashMap<usize, usize>>>,

    core_track_elements: Arc<Mutex<HashMap<ges::Clip, HashSet<ges::TrackElement>>>>,

    // Track signal handlers for control source value changes
    control_source_handlers: Arc<Mutex<Vec<SignalsHandler>>>,

    // Track property bindings so they can be removed when control bindings are added
    // Key: (replica_child, property_name) -> glib::Binding
    property_bindings: Arc<Mutex<HashMap<(glib::Object, String), glib::Binding>>>,
}

fn control_binding_property_name(control_binding: &gst::ControlBinding) -> String {
    format!(
        "{}::{}",
        control_binding.property_spec().owner_type().name(),
        control_binding.property_name()
    )
}

impl Replica {
    fn unwrap_timeline(&self) -> ges::Timeline {
        self.timeline.upgrade().unwrap()
    }

    fn create_replica_from_asset<T: glib::object::IsA<ges::TimelineElement> + StaticType>(
        &self,
        primary: ges::TimelineElement,
        skipped_properties: &[&str],
    ) -> Result<T, glib::BoolError> {
        let replica = primary
            .asset()
            .ok_or_else(|| {
                glib::bool_error!("Failed to get asset for replica creation for {primary:?}")
            })?
            .extract()
            .map_err(|e| {
                glib::bool_error!("Failed to extract asset for replica creation: {e:?}")
            })?;

        gst::debug!(CAT, "EXTRACTED ASSET {:?}", replica);
        let replica = replica
            .dynamic_cast::<T>()
            .expect("Extracted asset is of expected type");

        if let Err(e) = replica.set_name(Some(&format!(
            "replica{}_{}",
            self.id,
            primary.name().unwrap().as_str(),
        ))) {
            gst::warning!(CAT, "Failed to set name on clip replica: {e}");
        }

        for property in primary.list_properties() {
            if !property.flags().contains(glib::ParamFlags::WRITABLE)
                || property.flags().contains(glib::ParamFlags::CONSTRUCT_ONLY)
                || skipped_properties.contains(&property.name())
                || property.name() == "name"
            {
                // Skip non-writable or construct-only properties
                gst::log!(
                    CAT,
                    "Skipping property '{}' during binding (writable: {}, construct-only: {})",
                    property.name(),
                    property.flags().contains(glib::ParamFlags::WRITABLE),
                    property.flags().contains(glib::ParamFlags::CONSTRUCT_ONLY)
                );
                continue;
            }

            gst::log!(
                CAT,
                "Binding property '{}' from primary clip to replica clip",
                property.name()
            );
            primary
                .bind_property(property.name(), &replica, property.name())
                .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                .build();
        }

        Ok(replica)
    }

    /// Helper function to copy GObject properties with flexible property filtering
    fn create_replica_object<T: glib::object::IsA<glib::Object> + StaticType>(
        &self,
        source: &impl glib::object::IsA<glib::Object>,
        skipped_properties: &[&str],
    ) -> T {
        let source_obj = source.upcast_ref::<glib::Object>();
        let object_type = source_obj.type_();

        let mut builder = glib::Object::builder_with_type(object_type);

        // Get all properties from the source object
        let pspecs = source_obj.list_properties();

        for pspec in pspecs.iter() {
            let name = pspec.name();

            // Skip specified properties
            if skipped_properties.contains(&name) {
                continue;
            }

            // Skip non-readable properties
            if !pspec.flags().contains(glib::ParamFlags::READABLE)
                || !pspec.flags().contains(glib::ParamFlags::WRITABLE)
            {
                continue;
            }

            // Copy the property value, but modify identifiers
            let mut value = source_obj.property_value(name);

            if name == "name" {
                value = format!("replica_{}_{}", self.id, value.get::<&str>().unwrap()).to_value();
            }

            builder = builder.property(name, value);
        }

        builder.build().downcast::<T>().unwrap()
    }

    fn add_layer(
        &mut self,
        replica_timeline: &ges::Timeline,
        primary_layer: &ges::Layer,
    ) -> Result<(), glib::BoolError> {
        let replica_layer = self.create_replica_object::<ges::Layer>(primary_layer, &[]);

        // Disable auto-transition on replica layers since editing is disabled

        gst::debug!(
            CAT,
            "Adding layer with priority={:?} to replica timeline {:?}",
            replica_layer.priority(),
            replica_timeline.name()
        );

        #[allow(deprecated)]
        replica_timeline.add_layer(&replica_layer)?;
        self.add_mapping(primary_layer, &replica_layer);

        gst::debug!(
            CAT,
            "Copying clips {:?} to replica layer",
            primary_layer.clips()
        );
        for clip in primary_layer.clips() {
            self.layer_add_clip(&replica_layer, &clip)?;
        }

        Ok(())
    }

    fn add_track(
        &mut self,
        replica_timeline: &ges::Timeline,
        primary_track: &ges::Track,
    ) -> Result<(), glib::BoolError> {
        let track_replica =
            self.create_replica_object::<ges::Track>(primary_track, &["parent", "duration"]);
        gst::debug!(
            CAT,
            "Adding {:?} to replica timeline {:?}",
            track_replica.name(),
            replica_timeline.name()
        );
        replica_timeline.add_track(&track_replica)?;
        self.add_mapping(primary_track, &track_replica);
        Ok(())
    }

    fn layer_add_clip(
        &mut self,
        replica_layer: &ges::Layer,
        primary_clip: &ges::Clip,
    ) -> Result<(), glib::BoolError> {
        gst::debug!(
            CAT,
            "Primary has children {:?}",
            primary_clip.children(true)
        );
        let clip_replica = self.create_replica_from_asset::<ges::Clip>(
            primary_clip.clone().upcast(),
            &["layer", "tracks", "children", "timeline", "parent"],
        )?;

        gst::debug!(
            CAT,
            "Adding clip {:?} to replica layer with priority {:?}",
            clip_replica.name(),
            replica_layer.priority()
        );

        let timeline = self.unwrap_timeline();
        let core_track_elements = self.core_track_elements.clone();
        core_track_elements
            .lock()
            .unwrap()
            .insert(clip_replica.clone(), Default::default());
        // Ensure the clip's track is part of the timeline
        let select_track_id =
            timeline.connect_select_element_track(move |_timeline, clip, track_element| {
                core_track_elements
                    .lock()
                    .unwrap()
                    .get_mut(clip)
                    .expect("We just added the clip in core_track_elements in the same thread.")
                    .insert(track_element.clone());
                gst::debug!(
                    CAT,
                    "Selecting no track for track element {} during clip replication",
                    track_element.name().unwrap().as_str()
                );
                None
            });

        replica_layer.add_clip(&clip_replica)?;
        timeline.disconnect(select_track_id);

        // Now that we know what the core elements are, we can remove them from the clip replica
        for child in clip_replica.children(true) {
            clip_replica.remove(&child).unwrap();
        }

        self.add_mapping(primary_clip, &clip_replica);

        // so they can be re-added properly, along with effects and other non-core elements
        // ensuring that core elements are added first
        for track_element in primary_clip
            .children(false)
            .into_iter()
            .sorted_by_key(|te| !te.downcast_ref::<ges::TrackElement>().unwrap().is_core())
        {
            self.clip_add_track_element(primary_clip, track_element.downcast_ref().unwrap())?;
        }

        Ok(())
    }

    fn get_clip_core_element(
        &mut self,
        clip: &ges::Clip,
        core_element_primary: &ges::TrackElement,
    ) -> Option<ges::TrackElement> {
        self.core_track_elements
            .lock()
            .unwrap()
            .get(clip)
            .unwrap()
            .iter()
            .find(|element| element.asset() == core_element_primary.asset())
            .cloned()
    }

    fn clip_remove_track_element(
        &mut self,
        primary_clip: &ges::Clip,
        primary_track_element: &ges::TrackElement,
    ) -> Result<(), glib::BoolError> {
        let clip_replica = self.clip(primary_clip).ok_or_else(|| {
            glib::bool_error!(
                "Could not find replica clip for primary clip in track element addition propagation"
            )
        })?;

        let replica_track_element = self.take_mapping::<ges::TrackElement>(primary_track_element).ok_or_else(|| {
            glib::bool_error!("Could not find replica track element for primary track element {:?} in removal propagation: {:#?}",
                primary_track_element.as_ptr(),
                self.element_mappings)
        })?;

        clip_replica.remove(&replica_track_element).map_err(|e| {
            glib::bool_error!(
                "Failed to remove track element replica from clip replica: {:?}",
                e
            )
        })
    }

    fn clip_add_track_element(
        &mut self,
        primary_clip: &ges::Clip,
        primary_track_element: &ges::TrackElement,
    ) -> Result<(), glib::BoolError> {
        let clip_replica = self.clip(primary_clip).ok_or_else(|| {
            glib::bool_error!(
                "Could not find replica clip for primary clip in track element addition propagation"
            )
        })?;

        let replica_track_element = if let Some(replica) = self.track_element(primary_track_element)
        {
            replica
        } else {
            let replica_track_element = if primary_track_element.is_core() {
                let replica_track_element = self.get_clip_core_element(&clip_replica, primary_track_element).ok_or_else(|| {
                    glib::bool_error!(
                        "Could not find core element replica for primary core element in track element addition propagation: {:?}",
                          self.core_track_elements.lock().unwrap()
                    )
                })?;
                if let Err(e) = replica_track_element.set_name(Some(&format!(
                    "replica{}_{}",
                    self.id,
                    primary_track_element.name().unwrap().as_str(),
                ))) {
                    gst::warning!(CAT, "Failed to set name on core element replica: {e}");
                }

                replica_track_element
            } else {
                let replica_track_element = self.create_replica_from_asset::<ges::TrackElement>(
                    primary_track_element.clone().upcast(),
                    &[
                        "tracks", "children", "timeline", "parent", "inpoint", "start", "duration",
                    ],
                )?;

                replica_track_element.downcast().unwrap()
            };

            self.add_mapping(primary_track_element, &replica_track_element);

            replica_track_element
        };

        gst::log!(
            CAT,
            "track element replica {:?} in {:?}",
            replica_track_element.name(),
            clip_replica.name(),
        );

        clip_replica.add(&replica_track_element)?;

        // Bind children properties from primary to replica track element (except those with control bindings)
        for property in
            ges::prelude::TimelineElementExt::list_children_properties(primary_track_element)
        {
            // Skip non-writable/non-readable properties
            if !property.flags().contains(glib::ParamFlags::WRITABLE)
                || !property.flags().contains(glib::ParamFlags::READABLE)
            {
                continue;
            }

            let property_name = format!("{}::{}", property.owner_type().name(), property.name());

            // Check if property has a control binding
            // Try with full "ChildTypeName:property-name" format first, then fallback to simple name
            // (as per FIXME in GES about bindings_hashtable key format)
            let control_binding = primary_track_element
                .control_binding(&property_name)
                .map_or_else(
                    || {
                        (
                            property.name().to_string(),
                            primary_track_element.control_binding(property.name()),
                        )
                    },
                    |binding| (property_name.clone(), Some(binding)),
                );

            // Handle control binding if present, otherwise use property binding
            if let (property_name, Some(binding)) = control_binding {
                if let Err(e) = self.replicate_control_binding(
                    primary_track_element,
                    &replica_track_element,
                    &property_name,
                    &binding,
                ) {
                    gst::warning!(
                        CAT,
                        "Failed to replicate control binding for '{}': {:?}",
                        property_name,
                        e
                    );
                }

                continue;
            }

            // lookup_child to get the actual child GObject that owns this property
            let (primary_child, primary_pspec) =
                match ges::prelude::TimelineElementExt::lookup_child(
                    primary_track_element,
                    &property_name,
                ) {
                    Some((child, pspec)) => (child, pspec),
                    None => {
                        gst::warning!(
                            CAT,
                            "Could not lookup child for property '{}'",
                            property_name
                        );
                        continue;
                    }
                };

            let (replica_child, replica_pspec) =
                match ges::prelude::TimelineElementExt::lookup_child(
                    &replica_track_element,
                    &property_name,
                ) {
                    Some((child, pspec)) => (child, pspec),
                    None => {
                        gst::warning!(
                            CAT,
                            "Could not lookup child for property '{}' on replica",
                            property_name
                        );
                        continue;
                    }
                };

            gst::log!(
                CAT,
                "Binding child property '{}' from primary child to replica child",
                property_name
            );

            let binding = primary_child
                .bind_property(primary_pspec.name(), &replica_child, replica_pspec.name())
                .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                .build();

            // Store with GType::property_name format for uniqueness
            let full_property_name = format!(
                "{}::{}",
                replica_pspec.owner_type().name(),
                replica_pspec.name()
            );
            self.property_bindings
                .lock()
                .unwrap()
                .insert((replica_child.clone(), full_property_name), binding);
        }

        Ok(())
    }

    // Temporary helper to get all control points from a control source
    // Returns (timestamp, value) tuples since ControlPoint is not publicly exported
    // TODO: Remove this once gstreamer-rs merges the list_timed_values() binding
    fn list_timed_values(
        control_source: &gst_controller::InterpolationControlSource,
    ) -> Vec<(gst::ClockTime, f64)> {
        unsafe {
            let list: *mut glib::ffi::GList =
                gst_controller::ffi::gst_timed_value_control_source_get_all(
                    control_source.as_ptr() as *mut _,
                );

            let mut result = Vec::new();
            let mut current = list;

            while !current.is_null() {
                let point = (*current).data as *const gst_controller::ffi::GstControlPoint;
                if !point.is_null() {
                    let timestamp = gst::ClockTime::from_nseconds((*point).timestamp);
                    let value = (*point).value;
                    result.push((timestamp, value));
                }
                current = (*current).next;
            }

            glib::ffi::g_list_free(list);
            result
        }
    }

    fn replicate_control_binding(
        &mut self,
        _primary_track_element: &ges::TrackElement,
        replica_track_element: &ges::TrackElement,
        property_name: &str,
        binding: &gst::ControlBinding,
    ) -> Result<(), glib::BoolError> {
        gst::log!(
            CAT,
            "Replicating control binding for property '{}' from primary to replica",
            property_name
        );

        // Try to downcast to DirectControlBinding (most common type)
        let direct_binding = binding
            .downcast_ref::<gst_controller::DirectControlBinding>()
            .ok_or_else(|| {
                glib::bool_error!(
                    "Unsupported control binding type for property '{}': {:?}",
                    property_name,
                    binding.type_()
                )
            })?;

        // Get the control source from the primary binding
        let control_source = direct_binding
            .control_source()
            .ok_or_else(|| {
                glib::bool_error!(
                    "DirectControlBinding has no control source for property '{}'",
                    property_name
                )
            })?
            .downcast::<gst_controller::InterpolationControlSource>()
            .map_err(|_| {
                glib::bool_error!(
                    "Control source is not an InterpolationControlSource for property '{}'",
                    property_name
                )
            })?;

        // Determine if binding is absolute mode
        let absolute = direct_binding.is_absolute();

        let replica_control_source = self
            .create_replica_object::<gst_controller::InterpolationControlSource>(
                &control_source,
                &["parent"],
            );

        // Copy all keyframes from primary to replica
        replica_control_source.unset_all();
        for timed_value in control_source.list_control_points() {
            replica_control_source.set(timed_value.timestamp(), timed_value.value());
        }

        // Set the control source on the replica track element
        if !replica_track_element.set_control_source(
            &replica_control_source,
            property_name,
            if absolute {
                "direct-absolute"
            } else {
                "direct"
            },
        ) {
            return Err(glib::bool_error!(
                "Failed to set control source on replica track element for property '{}'",
                property_name
            ));
        }

        gst::debug!(
            CAT,
            "Successfully replicated control binding for property '{}' (absolute: {})",
            property_name,
            absolute
        );

        // Now set up signal handlers to track value changes on the primary control source
        // and replicate them to the replica control source
        let replica_control_source_weak = replica_control_source.downgrade();
        let property_name = property_name.to_string();

        // Store signal handlers so they persist
        self.control_source_handlers.lock().unwrap().push(
            SignalsHandler {
                object: control_source.upcast_ref::<glib::Object>().downgrade(),
                ids: Vec::new(),
            }
            .add(control_source.connect_value_added(glib::clone!(
                #[strong]
                replica_control_source_weak,
                #[strong]
                property_name,
                move |_source, timed_value| {
                    let Some(replica_source) = replica_control_source_weak.upgrade() else {
                        return;
                    };

                    gst::log!(
                        CAT,
                        "Replicating value-added for property '{}': timestamp={:?}, value={}",
                        property_name,
                        timed_value.timestamp(),
                        timed_value.value()
                    );

                    replica_source.set(timed_value.timestamp(), timed_value.value());
                }
            )))
            .add(control_source.connect_value_changed(glib::clone!(
                #[strong]
                replica_control_source_weak,
                #[strong]
                property_name,
                move |_source, timed_value| {
                    let Some(replica_source) = replica_control_source_weak.upgrade() else {
                        return;
                    };

                    gst::log!(
                        CAT,
                        "Replicating value-changed for property '{}': timestamp={:?}, value={}",
                        property_name,
                        timed_value.timestamp(),
                        timed_value.value()
                    );

                    replica_source.set(timed_value.timestamp(), timed_value.value());
                }
            )))
            .add(control_source.connect_value_removed(glib::clone!(
                #[strong]
                replica_control_source_weak,
                move |_source, timed_value| {
                    let Some(replica_source) = replica_control_source_weak.upgrade() else {
                        return;
                    };

                    gst::log!(
                        CAT,
                        "Replicating value-removed for property '{}': timestamp={:?}",
                        property_name,
                        timed_value.timestamp()
                    );

                    replica_source.unset(timed_value.timestamp());
                }
            ))),
        );

        Ok(())
    }

    fn add_mapping(
        &mut self,
        primary: &impl glib::object::IsA<glib::Object>,
        replica: &impl glib::object::IsA<glib::Object>,
    ) {
        self.element_mappings
            .lock()
            .unwrap()
            .insert(primary.as_ptr() as usize, replica.as_ptr() as usize);
    }

    fn take_mapping<T>(&mut self, primary: &impl glib::object::IsA<glib::Object>) -> Option<T>
    where
        T: glib::object::IsA<glib::Object> + StaticType,
    {
        unsafe {
            self.element_mappings
                .lock()
                .unwrap()
                .remove(&(primary.as_ptr() as usize))
                .map(|replica_ptr| {
                    from_glib_none::<*mut gst::glib::gobject_ffi::GObject, gst::glib::Object>(
                        replica_ptr as *mut gst::glib::gobject_ffi::GObject,
                    )
                    .downcast::<T>()
                    .unwrap()
                })
        }
    }

    fn get_replica<T>(&self, primary: &impl glib::object::IsA<glib::Object>) -> Option<T>
    where
        T: glib::object::IsA<glib::Object> + StaticType,
    {
        unsafe {
            self.element_mappings
                .lock()
                .unwrap()
                .get(&(primary.as_ptr() as _))
                .map(|replica_ptr| {
                    from_glib_none::<*mut gst::glib::gobject_ffi::GObject, gst::glib::Object>(
                        *replica_ptr as *mut gst::glib::gobject_ffi::GObject,
                    )
                    .downcast::<T>()
                    .unwrap()
                })
        }
    }

    fn remove_track(&mut self, primary_track: &ges::Track) -> Result<(), glib::BoolError> {
        self.unwrap_timeline().remove_track(
            &self
                .take_mapping::<ges::Track>(primary_track)
                .ok_or_else(|| {
                    glib::bool_error!(
                        "Failed to find replica {:?} {} during removal ({:?})",
                        primary_track.name(),
                        primary_track.as_ptr() as usize,
                        self.element_mappings
                    )
                })?,
        )
    }

    fn remove_clip(&mut self, primary_clip: &ges::Clip) -> Result<(), glib::BoolError> {
        let clip_replica: ges::Clip = self.take_mapping(primary_clip).ok_or_else(|| {
            glib::bool_error!(
                "Failed to find replica {:?} {} during removal ({:?})",
                primary_clip.name(),
                primary_clip.as_ptr() as usize,
                self.element_mappings
            )
        })?;

        self.core_track_elements
            .lock()
            .unwrap()
            .remove(&clip_replica);

        let replica_layer = clip_replica.layer().ok_or_else(|| {
            glib::bool_error!("Failed to get layer of replica clip during removal")
        })?;

        replica_layer.remove_clip(&clip_replica)
    }

    fn track_element(
        &self,
        primary_track_element: &ges::TrackElement,
    ) -> Option<ges::TrackElement> {
        self.get_replica(primary_track_element)
    }

    fn clip(&self, primary_clip: &ges::Clip) -> Option<ges::Clip> {
        self.get_replica(primary_clip)
    }

    fn layer(&self, primary_layer: &ges::Layer) -> Option<ges::Layer> {
        self.get_replica(primary_layer)
    }

    fn remove_layer(&mut self, primary_layer: &ges::Layer) -> Result<(), glib::BoolError> {
        self.unwrap_timeline().remove_layer(
            &self
                .take_mapping::<ges::Layer>(primary_layer)
                .ok_or_else(|| {
                    glib::bool_error!(
                        "Failed to find replica {} during removal ({:?})",
                        primary_layer.as_ptr() as usize,
                        self.element_mappings
                    )
                })?,
        )
    }
}

#[derive(Debug)]
struct SignalsHandler {
    object: WeakRef<glib::Object>,
    ids: Vec<SignalHandlerId>,
}

impl SignalsHandler {
    fn new(obj: &impl glib::object::IsA<glib::Object>) -> Self {
        Self {
            object: obj.upcast_ref().downgrade(),
            ids: Default::default(),
        }
    }
    fn add(mut self, id: SignalHandlerId) -> SignalsHandler {
        self.ids.push(id);

        self
    }
}

impl Drop for SignalsHandler {
    fn drop(&mut self) {
        let Some(object) = self.object.upgrade() else {
            return;
        };

        for id in self.ids.drain(..) {
            object.disconnect(id);
        }
    }
}

// Primary storage: primary_id -> (primary_timeline, Vec<weak_refs_to_replicas>)
#[derive(Debug)]
struct PrimaryInner {
    timeline: ges::Timeline,
    replicas: Vec<Replica>,
    n_replicas: u64,

    signal_handlers: Vec<SignalsHandler>,
}

impl PrimaryInner {
    fn add(&mut self, replica: Replica) {
        self.replicas.push(replica);
        self.cleanup();
        self.n_replicas += 1;
    }

    fn cleanup(&mut self) {
        self.replicas
            .retain(|replica| replica.timeline.upgrade().is_some());
    }

    fn push_signals_handler(&mut self, handler: SignalsHandler) {
        self.signal_handlers.push(handler);
    }
}

#[derive(Debug, Clone)]
struct Primary(Arc<Mutex<PrimaryInner>>);

impl From<Arc<Mutex<PrimaryInner>>> for Primary {
    fn from(inner: Arc<Mutex<PrimaryInner>>) -> Self {
        Primary(inner)
    }
}

impl From<Primary> for Arc<Mutex<PrimaryInner>> {
    fn from(primary: Primary) -> Self {
        primary.0
    }
}

impl AsRef<Arc<Mutex<PrimaryInner>>> for Primary {
    fn as_ref(&self) -> &Arc<Mutex<PrimaryInner>> {
        &self.0
    }
}

impl std::ops::Deref for Primary {
    type Target = Arc<Mutex<PrimaryInner>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Primary {
    fn disconnect_signals_for(&self, removed_object: &impl glib::object::IsA<glib::Object>) {
        self.lock().unwrap().signal_handlers.retain(|handler| {
            let Some(obj) = handler.object.upgrade() else {
                return false;
            };

            if &obj == removed_object {
                return false;
            }

            true
        });
    }

    fn iter_replicas(self, timeline_guards: Vec<TimelineLockGuard>) -> ReplicasIter {
        let primary_inner = self.0.lock().unwrap();

        let replicas = primary_inner.replicas.clone().into_iter();
        drop(primary_inner);
        return ReplicasIter {
            replicas,
            _timeline_guards: timeline_guards,
            primary: self,
        };
    }
}

#[derive(Debug, Default)]
pub struct SubtimelinePrimaryManager {
    primaries: Mutex<HashMap<String, Primary>>,
}

// SAFETY: GESTimeline is not Send+Sync but we know that there will be runtime
// check for APIs that are not thread safe.
unsafe impl Send for SubtimelinePrimaryManager {}
unsafe impl Sync for SubtimelinePrimaryManager {}

/// Iterator over replicas with their associated primary info
struct ReplicasIter {
    replicas: std::vec::IntoIter<Replica>,
    // Keep timelines alive for the duration of iteration to prevent weak refs from becoming invalid
    _timeline_guards: Vec<TimelineLockGuard>,
    primary: Primary,
}

impl ReplicasIter {
    /// Get the primary info associated with these replicas
    fn primary(&self) -> Primary {
        self.primary.clone()
    }
}

impl Iterator for ReplicasIter {
    type Item = Replica;

    fn next(&mut self) -> Option<Self::Item> {
        self.replicas.next()
    }
}

impl SubtimelinePrimaryManager {
    pub fn register_primary_internal(
        &self,
        primary_id: &str,
        timeline: &ges::Timeline,
    ) -> Result<(), glib::Error> {
        // Validate timeline state - must not have parent
        if timeline.parent().is_some() {
            return Err(glib::Error::new(
                gst::CoreError::Failed,
                "Timeline is already in use (has parent) and cannot be registered as primary",
            ));
        }

        let mut primaries = self.primaries.lock().unwrap();
        if primaries.contains_key(primary_id) {
            return Err(glib::Error::new(
                gst::CoreError::Failed,
                &format!("Primary with id '{}' already exists", primary_id),
            ));
        }

        gst::info!(
            CAT,
            imp = self,
            "Registering {timeline:?} as primary with id: {}",
            primary_id
        );

        // Insert primary with empty replica list
        let primary = Primary(Arc::new(Mutex::new(PrimaryInner {
            timeline: timeline.clone(),
            replicas: Vec::new(),
            n_replicas: 0,
            signal_handlers: Vec::new(),
        })));
        primaries.insert(primary_id.to_string(), primary.clone());

        // Set up change tracking for the primary with safeguards in place
        self.setup_primary_change_tracking(primary_id, primary, timeline);
        self.obj()
            .emit_by_name::<()>("subtimeline-primary-registered", &[&primary_id, timeline]);

        Ok(())
    }

    pub fn unregister_primary(&self, primary_id: &str) -> Result<(), glib::Error> {
        let mut primaries = self.primaries.lock().unwrap();

        let primary = if let Some(primary_info_lock) = primaries.remove(primary_id) {
            primary_info_lock
        } else {
            return Err(glib::Error::new(
                gst::CoreError::Failed,
                &format!("Primary with id '{}' not found", primary_id),
            ));
        };

        gst::info!(CAT, imp = self, "Unregistered primary: {primary_id}");
        let primary_timeline = {
            let primary_inner = primary.lock().unwrap();
            primary_inner.timeline.clone()
        };
        drop(primary);

        let obj = self.obj();
        obj.emit_by_name::<()>(
            "subtimeline-primary-unregistered",
            &[&primary_id, &primary_timeline],
        );

        Ok(())
    }

    fn primary(&self, primary_id: &str) -> Option<(Primary, Vec<TimelineLockGuard>)> {
        let primaries = self.primaries.lock().unwrap();
        let Some(primary) = primaries.get(primary_id).cloned() else {
            return None;
        };

        let mut primary_inner = primary.lock().unwrap();
        let mut timeline_locks = vec![TimelineLockGuard::new(&primary_inner.timeline)];
        primary_inner.replicas.retain(|replica| {
            if let Some(replica_timeline) = replica.timeline.upgrade() {
                timeline_locks.push(TimelineLockGuard::new(&replica_timeline));
                true
            } else {
                false
            }
        });
        drop(primary_inner);

        Some((primary, timeline_locks))
    }

    pub fn primary_timeline(&self, primary_id: &str) -> Option<ges::Timeline> {
        let primaries = self.primaries.lock().unwrap();
        primaries
            .get(primary_id)
            .map(|primary_inner| primary_inner.clone().lock().unwrap().timeline.clone())
    }

    pub fn make_replica(
        &self,
        primary_id: &str,
        replica_timeline: &ges::Timeline,
    ) -> Result<(), glib::Error> {
        let (primary, _timelines_lock) = self.primary(primary_id).ok_or_else(|| {
            glib::Error::new(
                gst::CoreError::Failed,
                &format!("Primary '{}' not found", primary_id),
            )
        })?;

        let mut primary_inner = primary.lock().unwrap();

        let primary_timeline = primary_inner.timeline.clone();
        let _replica_lock = TimelineLockGuard::new(replica_timeline);

        let mut replica = Replica {
            id: primary_inner.n_replicas,
            timeline: replica_timeline.downgrade(),
            element_mappings: Default::default(),

            core_track_elements: Arc::new(Mutex::new(Default::default())),
            control_source_handlers: Arc::new(Mutex::new(Vec::new())),
            property_bindings: Arc::new(Mutex::new(HashMap::new())),
        };

        gst::debug!(
            CAT,
            imp = self,
            "Making replica {:?} from primary: {}",
            replica.id,
            primary_id,
        );

        // Clear any existing content from the target timeline
        for layer in replica_timeline.layers() {
            let _ = replica_timeline.remove_layer(&layer);
        }
        for track in replica_timeline.tracks() {
            let _ = replica_timeline.remove_track(&track);
        }

        self.copy_timeline(&mut primary_inner, &mut replica)
            .map_err(|e| {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to copy timeline from primary '{}' to replica: {}",
                    primary_id,
                    e
                );
                glib::Error::new(gst::CoreError::Failed, &e.to_string())
            })?;

        primary_inner.add(replica);

        gst::info!(
            CAT,
            imp = self,
            "Successfully made replica from primary '{}'",
            primary_id
        );

        // Emit signal to notify that a new replica has been created
        self.obj().emit_by_name::<()>(
            "new-timeline-replica",
            &[&primary_id, &primary_timeline, replica_timeline],
        );

        Ok(())
    }

    pub fn is_primary(&self, timeline: &ges::Timeline) -> bool {
        self.primaries
            .lock()
            .unwrap()
            .values()
            .find(|primary_inner| &primary_inner.lock().unwrap().timeline == timeline)
            .is_some()
    }

    pub fn get_primary(&self, primary_id: &str) -> Option<ges::Timeline> {
        self.primaries
            .lock()
            .unwrap()
            .get(primary_id)
            .map(|primary_inner| primary_inner.lock().unwrap().timeline.clone())
    }

    pub fn get_primary_id(&self, timeline: &ges::Timeline) -> Option<String> {
        self.primaries
            .lock()
            .unwrap()
            .iter()
            .find_map(|(id, primary_inner)| {
                if &primary_inner.lock().unwrap().timeline == timeline {
                    Some(id.clone())
                } else {
                    None
                }
            })
    }

    fn copy_timeline(
        &self,
        primary_inner: &mut PrimaryInner,
        replica: &mut Replica,
    ) -> Result<(), glib::BoolError> {
        gst::debug!(
            CAT,
            imp = self,
            "Starting copy of timeline content with element mapping tracking"
        );

        // Disable editing APIs on replica from the start
        let replica_timeline = replica.unwrap_timeline();
        replica_timeline.disable_edit_apis(true);
        self.copy_timeline_properties(&primary_inner.timeline, &replica_timeline, replica.id);

        // Copy all tracks from primary_timeline to replica_timeline with mapping
        for track in primary_inner.timeline.tracks() {
            replica.add_track(&replica_timeline, &track)?;
        }

        // Copy all layers from primary_timeline to replica with mapping
        for layer in primary_inner.timeline.layers() {
            replica.add_layer(&replica_timeline, &layer)?;
        }

        gst::info!(
            CAT,
            imp = self,
            "Completed timeline copy with element mapping for replica_id: {}",
            replica.id
        );

        Ok(())
    }

    fn propagate_timeline_commit(&self, primary_id: &str) {
        for replica in self.iter_replicas(primary_id) {
            gst::info!(
                CAT,
                imp = self,
                "Committing replica_id: {} for primary_id: {}",
                replica.id,
                primary_id
            );
            replica.unwrap_timeline().commit();
        }
    }

    fn setup_primary_change_tracking(
        &self,
        primary_id: &str,
        primary: Primary,
        primary_timeline: &ges::Timeline,
    ) {
        let primary_id = primary_id.to_string();
        gst::debug!(
            CAT,
            imp = self,
            "Setting up change tracking for primary: {}",
            primary_id
        );
        let mut primary_inner = primary.lock().unwrap();

        primary_inner.timeline.connect_closure(
            "commit",
            false,
            glib::closure!(
                #[weak(rename_to = this)]
                self,
                #[strong]
                primary_id,
                move |_timeline: &ges::Timeline| {
                    this.propagate_timeline_commit(&primary_id);
                }
            ),
        );

        primary_inner.push_signals_handler(
            SignalsHandler::new(primary_timeline)
                .add(primary_timeline.connect_track_added(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |timeline, track| {
                        this.propagate_track_added(&primary_id, timeline, track);
                    }
                )))
                .add(primary_timeline.connect_track_removed(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |_timeline, track| {
                        this.propagate_track_removed(&primary_id, track);
                    }
                )))
                .add(primary_timeline.connect_layer_added(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |timeline, layer| {
                        this.propagate_layer_added(&primary_id, timeline, layer);
                    }
                )))
                .add(primary_timeline.connect_layer_removed(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |timeline, layer| {
                        this.propagate_layer_removed(&primary_id, timeline, layer);
                    }
                ))),
        );

        // Set up tracking for existing layers
        for layer in primary_timeline.layers() {
            self.setup_layer_tracking(&primary_id, &layer, &mut primary_inner);
        }

        gst::debug!(
            CAT,
            imp = self,
            "Change tracking setup complete for primary: {}",
            primary_id
        );
    }

    fn setup_layer_tracking(
        &self,
        primary_id: &str,
        layer: &ges::Layer,
        primary_inner: &mut PrimaryInner,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Setting up layer tracking for primary: {} layer priority: {}",
            primary_id,
            layer.priority()
        );

        let layer_priority = layer.priority();
        let primary_id = primary_id.to_string();

        primary_inner.push_signals_handler(
            SignalsHandler::new(layer)
                .add(layer.connect_clip_added(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |layer, clip| {
                        if clip.is_moving_between_layers() {
                            gst::debug!(
                        CAT,
                        "Ignoring clip-added signal for clip {:?} which is moving between layers",
                        clip.name()
                    );
                            return;
                        }
                        this.propagate_clip_added(&primary_id, layer, clip);
                    }
                )))
                .add(layer.connect_clip_removed(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |layer, clip| {
                        if clip.is_moving_between_layers() {
                            gst::debug!(
                        CAT,
                        "Ignoring clip-removed signal for clip {:?} which is moving between layers",
                        clip.name()
                    );
                            return;
                        }
                        this.propagate_clip_removed(&primary_id, layer, clip);
                    }
                ))),
        );

        for clip in layer.clips() {
            self.clip_added(&primary_id, &layer, &clip, primary_inner);

            for element in clip.children(false) {
                self.setup_control_binding_tracking(
                    &primary_id,
                    element.downcast_ref::<ges::TrackElement>().unwrap(),
                    primary_inner,
                );
            }
        }

        gst::debug!(
            CAT,
            imp = self,
            "Layer tracking setup complete for priority: {}",
            layer_priority
        );
    }

    fn propagate_layer_added(
        &self,
        primary_id: &str,
        _primary: &ges::Timeline,
        layer: &ges::Layer,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating layer addition from primary: {}",
            primary_id
        );

        let iter = self.iter_replicas(primary_id);
        let primary = iter.primary();
        let mut primary_inner = primary.lock().unwrap();

        for mut replica in iter {
            replica.add_layer(&replica.unwrap_timeline(), layer).ok();
        }

        self.setup_layer_tracking(primary_id, layer, &mut primary_inner);
    }

    fn propagate_layer_removed(
        &self,
        primary_id: &str,
        _primary: &ges::Timeline,
        layer: &ges::Layer,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating layer removal from primary: {}",
            primary_id
        );

        let layer_priority = layer.priority();
        let (primary, timeline_locks) = self
            .primary(primary_id)
            .expect("Propagating 'layer-removed' on a timeline that is not registered as primary");

        primary.disconnect_signals_for(layer);
        gst::debug!(
            CAT,
            imp = self,
            "Cleaned up layer tracking for priority: {}",
            layer_priority
        );

        for mut replica in primary.iter_replicas(timeline_locks) {
            if let Err(e) = replica.remove_layer(&layer) {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to remove layer from replica during propagation: {e:?}",
                );
            }
        }
    }

    fn propagate_track_added(
        &self,
        primary_id: &str,
        timeline_primary: &ges::Timeline,
        track: &ges::Track,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating track addition from primary: {}",
            primary_id
        );

        for mut replica in self.iter_replicas(primary_id) {
            let _ = replica.add_track(timeline_primary, &track);
        }
    }

    fn propagate_track_removed(&self, primary_id: &str, track: &ges::Track) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating track removal from primary: {}",
            primary_id
        );

        let iter = self.iter_replicas(primary_id);
        let primary = iter.primary();
        for mut replica in iter {
            replica.remove_track(&track).ok();
        }
        primary.disconnect_signals_for(track);
    }

    fn propagate_clip_added(&self, primary_id: &str, layer: &ges::Layer, clip: &ges::Clip) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating clip addition from primary: {}",
            primary_id
        );

        let (primary, _timeline_locks) = self
            .primary(primary_id)
            .expect("Propagating 'clip-added' on a timeline that is not registered as primary");

        let mut primary_inner = primary.lock().unwrap();
        self.clip_added(primary_id, layer, clip, &mut primary_inner);
    }

    fn clip_added(
        &self,
        primary_id: &str,
        layer: &ges::Layer,
        clip: &ges::Clip,
        primary_inner: &mut PrimaryInner,
    ) {
        for replica in primary_inner.replicas.iter_mut() {
            let Some(replica_layer) = replica.layer(layer) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Could not find replica layer for primary layer in clip addition propagation"
                );
                continue;
            };
            if let Err(e) = replica.layer_add_clip(&replica_layer, &clip) {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to add clip to replica layer during propagation: {e:?}",
                );
            }
        }

        let primary_id = primary_id.to_string();
        primary_inner.push_signals_handler(
            SignalsHandler::new(clip)
                .add(clip.connect_child_added(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |clip, child| {
                        gst::debug!(
                            CAT,
                            imp = this,
                            "Child added to clip '{:?}': {:?}",
                            clip.name(),
                            child.name()
                        );
                        this.propagate_clip_child_added(&primary_id, clip, child);
                    }
                )))
                .add(clip.connect_child_removed(glib::clone!(
                    #[weak(rename_to = this)]
                    self,
                    #[strong]
                    primary_id,
                    move |clip, child| {
                        gst::debug!(
                            CAT,
                            imp = this,
                            "Child removed from clip '{:?}': {:?}",
                            clip.name(),
                            child.name()
                        );
                        this.propagate_clip_child_removed(&primary_id, clip, child);
                    }
                )))
                .add(clip.connect_notify(
                    None,
                    glib::clone!(
                        #[weak(rename_to = this)]
                        self,
                        #[strong]
                        primary_id,
                        move |clip, param_spec| {
                            // Handle layer changes specially since 'layer'
                            // we need to handle 'moved_layer' in a special way
                            if param_spec.name() == "layer" {
                                gst::debug!(
                                    CAT,
                                    imp = this,
                                    "Clip {:?} layer changed, propagating...",
                                    clip.name()
                                );
                                this.propagate_clip_moved_layer(&primary_id, clip);
                                return;
                            }

                            if !param_spec.flags().contains(
                                glib::ParamFlags::WRITABLE | glib::ParamFlags::CONSTRUCT_ONLY,
                            ) {
                                // Skip not writable properties
                                return;
                            }
                            this.propagate_clip_property_changed(&primary_id, clip, param_spec);
                        }
                    ),
                )),
        );
    }

    fn propagate_clip_child_added(
        &self,
        primary_id: &str,
        primary_clip: &ges::Clip,
        child: &ges::TimelineElement,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating child addition from primary: {}, clip: {:?}, child: {:?}",
            primary_id,
            primary_clip.name(),
            child.name()
        );

        let clip_name = primary_clip.name();
        for mut replica in self.iter_replicas(primary_id) {
            if let Err(e) =
                replica.clip_add_track_element(primary_clip, child.downcast_ref().unwrap())
            {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to add track element to replica clip '{clip_name:?}' during propagation: {e:?}",
                );
            }
        }

        // Set up control binding tracking for the track element
        let (primary, _timeline_locks) = self.primary(primary_id).expect(
            "Setting up control binding tracking on a timeline that is not registered as primary",
        );

        self.setup_control_binding_tracking(
            primary_id,
            child.downcast_ref().unwrap(),
            &mut primary.lock().unwrap(),
        );
    }

    fn propagate_clip_child_removed(
        &self,
        primary_id: &str,
        primary_clip: &ges::Clip,
        child: &ges::TimelineElement,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating child removal from primary: {}, clip: {:?}, child: {:?}",
            primary_id,
            primary_clip.name(),
            child.name()
        );

        let clip_name = primary_clip.name();
        let iter = self.iter_replicas(primary_id);
        let primary = iter.primary();
        for mut replica in iter {
            if let Err(e) =
                replica.clip_remove_track_element(primary_clip, child.downcast_ref().unwrap())
            {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to remove track element to replica clip '{clip_name:?}' during propagation: {e:?}",
                );
            }
        }

        primary.disconnect_signals_for(child);
    }

    fn setup_control_binding_tracking(
        &self,
        primary_id: &str,
        primary_track_element: &ges::TrackElement,
        primary_inner: &mut PrimaryInner,
    ) {
        gst::log!(
            CAT,
            imp = self,
            "Setting up control binding tracking for primary track element '{:?}'",
            primary_track_element.name()
        );

        let primary_id = primary_id.to_string();

        // Connect to control-binding-added signal
        let handler_id_added = primary_track_element.connect_control_binding_added(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[strong]
            primary_id,
            move |primary_element, control_binding| {
                let full_property_name = control_binding_property_name(control_binding);
                gst::debug!(
                    CAT,
                    imp = this,
                    "Control binding added for property '{}' on primary, propagating to all replicas",
                    full_property_name
                );

                for replica in this.iter_replicas(&primary_id) {
                    let Some(replica_element) = replica.track_element(primary_element) else {
                        gst::warning!(CAT, "No replica element for {primary_element:?}");
                        continue;
                    };

                    // Extract the bare property name (after ::) for lookup_child
                    let property_name = full_property_name.split("::").last().unwrap();

                    // Look up the replica child for this property
                    let (replica_child, _) = match ges::prelude::TimelineElementExt::lookup_child(
                        &replica_element,
                        property_name,
                    ) {
                        Some((child, pspec)) => (child, pspec),
                        None => {
                            gst::warning!(
                                CAT,
                                imp = this,
                                "Could not lookup child for property '{}' on replica during control binding tracking",
                                property_name
                            );
                            continue;
                        }
                    };

                    // Remove the existing property binding with fallback
                    let binding = replica.property_bindings.lock().unwrap().remove(&(replica_child.clone(), full_property_name.clone()))
                        .or_else(|| replica.property_bindings.lock().unwrap().remove(&(replica_child.clone(), property_name.to_string())));

                    if let Some(binding) = binding {
                        binding.unbind();
                        gst::debug!(
                            CAT,
                            imp = this,
                            "Removed property binding for property '{}'",
                            full_property_name
                        );
                    }

                    // Now replicate the control binding to the replica
                    if let Some(direct_binding) = control_binding.downcast_ref::<gst_controller::DirectControlBinding>() {
                        if let Some(control_source) = direct_binding.control_source() {
                            if let Ok(interp_source) = control_source.downcast::<gst_controller::InterpolationControlSource>() {
                                // Create a replica control source
                                let replica_control_source = gst_controller::InterpolationControlSource::new();
                                replica_control_source.set_mode(interp_source.mode());

                                // Copy all keyframes
                                for (timestamp, value) in Replica::list_timed_values(&interp_source) {
                                    replica_control_source.set(timestamp, value);
                                }

                                // Set the control source on the replica
                                let absolute = direct_binding.is_absolute();
                                replica_element.set_control_source(
                                    &replica_control_source,
                                    property_name,
                                    if absolute { "direct-absolute" } else { "direct" },
                                );

                                gst::debug!(
                                    CAT,
                                    imp = this,
                                    "Added control binding for property '{}' on replica",
                                    full_property_name
                                );
                            }
                        }
                    }
                }
            }
        ));

        // Connect to control-binding-removed signal
        let handler_id_removed = primary_track_element.connect_control_binding_removed(glib::clone!(
            #[weak(rename_to = this)]
            self,
            #[strong]
            primary_id,
            move |primary_element, control_binding| {
                let full_property_name = control_binding_property_name(control_binding);
                gst::debug!(
                    CAT,
                    imp = this,
                    "Control binding removed for property '{}' on primary, propagating to all replicas",
                    full_property_name
                );

                for replica in this.iter_replicas(&primary_id) {
                    let Some(replica_element) = replica.track_element(primary_element) else {
                        continue;
                    };

                    // Extract the bare property name (after ::) for removal and lookup
                    let property_name = full_property_name.split("::").last().unwrap();

                    // Remove the control binding from the replica
                    replica_element.remove_control_binding(property_name).ok();

                    // Re-establish the property binding
                    let (primary_child, primary_pspec) = match ges::prelude::TimelineElementExt::lookup_child(
                        primary_element,
                        property_name,
                    ) {
                        Some((child, pspec)) => (child, pspec),
                        None => {
                            gst::warning!(
                                CAT,
                                imp = this,
                                "Could not lookup primary child for property '{}' during control binding removal",
                                property_name
                            );
                            continue;
                        }
                    };

                    let (replica_child, replica_pspec) = match ges::prelude::TimelineElementExt::lookup_child(
                        &replica_element,
                        property_name,
                    ) {
                        Some((child, pspec)) => (child, pspec),
                        None => {
                            gst::warning!(
                                CAT,
                                imp = this,
                                "Could not lookup replica child for property '{}' during control binding removal",
                                property_name
                            );
                            continue;
                        }
                    };

                    // Re-create the property binding
                    let binding = primary_child
                        .bind_property(primary_pspec.name(), &replica_child, replica_pspec.name())
                        .flags(glib::BindingFlags::DEFAULT | glib::BindingFlags::SYNC_CREATE)
                        .build();

                    // Store it again with full format
                    replica.property_bindings.lock().unwrap().insert(
                        (replica_child, full_property_name.clone()),
                        binding,
                    );

                    gst::debug!(
                        CAT,
                        imp = this,
                        "Restored property binding for property '{}'",
                        full_property_name
                    );
                }
            }
        ));

        primary_inner.push_signals_handler(
            SignalsHandler::new(primary_track_element)
                .add(handler_id_added)
                .add(handler_id_removed),
        );

        gst::log!(
            CAT,
            imp = self,
            "Control binding tracking setup complete for track element '{:?}'",
            primary_track_element.name()
        );
    }

    fn propagate_clip_removed(&self, primary_id: &str, layer: &ges::Layer, clip: &ges::Clip) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating layer::clip-removed from primary: {}",
            primary_id
        );

        for mut replica in self.iter_replicas(primary_id) {
            if let Some(_replica_timeline) = replica.timeline.upgrade() {
                // Use direct mapping to find the corresponding layer and clip
                let Some(_replica_layer) = replica.layer(layer) else {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "Could not find replica layer for primary layer in clip removal propagation"
                    );
                    continue;
                };
                if let Err(e) = replica.remove_clip(clip) {
                    gst::error!(
                        CAT,
                        imp = self,
                        "Failed to remove clip from replica layer during propagation: {e:?}",
                    );
                }
            }
        }
    }

    fn iter_replicas(&self, primary_id: &str) -> ReplicasIter {
        let (primary, timeline_locks) = self.primary(primary_id).expect(&format!(
            "Trying to iterate replicas for a timeline that is not registered as primary",
        ));

        primary.iter_replicas(timeline_locks)
    }

    fn propagate_clip_moved_layer(&self, primary_id: &str, primary_clip: &ges::Clip) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating clip layer move from primary: {}, clip: {:?}",
            primary_id,
            primary_clip.name()
        );
        let layer = match primary_clip.layer() {
            Some(l) => l,
            None => {
                gst::log!(
                    CAT,
                    imp = self,
                    "layer::clip-removed should be called for {:?} since it is no \
                        longer on a layer",
                    primary_clip.name()
                );

                return;
            }
        };

        let clip_name = primary_clip.name();
        for replica in self.iter_replicas(primary_id) {
            let replica_clip = match replica.clip(primary_clip) {
                Some(c) => c,
                None => {
                    gst::info!(
                        CAT,
                        imp = self,
                        "Could not find replica clip '{clip_name:?} for property propagation",
                    );
                    continue;
                }
            };
            let Some(replica_layer) = replica.layer(&layer) else {
                gst::warning!(
                    CAT,
                    imp = self,
                    "Could not find replica clip '{clip_name:?}' replicate clip move between layers",
                );
                continue;
            };
            if let Err(e) = replica_clip.move_to_layer(&replica_layer) {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to move replica clip '{clip_name:?}' to new layer: {e:?}",
                );
            }
        }
    }

    fn propagate_clip_property_changed(
        &self,
        primary_id: &str,
        clip: &ges::Clip,
        param_spec: &glib::ParamSpec,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "Propagating clip property change from primary: {}, property: {}",
            primary_id,
            param_spec.name()
        );

        let property_name = param_spec.name();
        let clip_name = clip.name();

        for replica in self.iter_replicas(primary_id) {
            // Find matching clip in each replica using element mappings
            let replica_clip = match replica.clip(clip) {
                Some(c) => c,
                None => {
                    gst::info!(
                        CAT,
                        imp = self,
                        "Could not find replica clip '{:?}' for property propagation",
                        clip_name
                    );
                    continue;
                }
            };
            let value = clip.property_value(property_name);
            replica_clip.set_property(property_name, &value);
            gst::debug!(
                CAT,
                imp = self,
                "Propagated property '{}' from clip '{:?}': {:?}",
                property_name,
                clip_name,
                value
            );
        }
    }

    fn copy_timeline_properties(
        &self,
        primary: &ges::Timeline,
        replica: &ges::Timeline,
        replica_id: u64,
    ) {
        // For timelines, we want to skip editing-related properties since editing is disabled
        let skipped_timeline_properties = &[
            "auto-transition",   // Editing feature, should be disabled
            "snapping-distance", // Editing feature, should be disabled
        ];

        // Copy timeline properties using the helper, but apply them to the existing replica
        // Since we already have a timeline object, we need to copy properties manually
        let primary_obj = primary.upcast_ref::<glib::Object>();
        let pspecs = primary_obj.list_properties();

        for pspec in pspecs.iter() {
            let name = pspec.name();

            // Skip specified properties
            if skipped_timeline_properties.contains(&name) {
                continue;
            }

            // Skip parent property (managed by container)
            if name == "parent" {
                continue;
            }

            // Skip read-only properties
            if !pspec.flags().contains(glib::ParamFlags::READABLE)
                || !pspec.flags().contains(glib::ParamFlags::WRITABLE)
            {
                continue;
            }

            // Copy the property value, but modify identifiers
            let mut value = primary_obj.property_value(name);

            if name == "name" {
                if let Ok(original_name) = value.get::<Option<String>>() {
                    let replica_name = original_name
                        .map(|n| format!("{}_replica_{}", n, replica_id))
                        .or_else(|| Some(format!("timeline_replica_{}", replica_id)));
                    value = replica_name.to_value();
                }
            }

            // Set the property on the replica
            replica.set_property(name, value);
        }
    }

    // Internal method for action signal implementation
    pub fn register_primary(
        &self,
        primary_id: &str,
        timeline: &ges::Timeline,
    ) -> Result<(), glib::Error> {
        self.obj()
            .emit_by_name::<Option<glib::Error>>(
                "register-subtimeline-primary",
                &[&primary_id, &timeline],
            )
            .map_or(Ok(()), Err)
    }

    // Internal method for action signal implementation
    fn unregister_primary_internal(&self, primary_id: &str) -> Option<glib::Error> {
        self.unregister_primary(primary_id).err()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for SubtimelinePrimaryManager {
    const NAME: &'static str = "SubtimelinePrimaryManager";
    type Type = super::SubtimelinePrimaryManager;
    type ParentType = gst::Object;
}

impl ObjectImpl for SubtimelinePrimaryManager {
    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: std::sync::OnceLock<Vec<glib::subclass::Signal>> =
            std::sync::OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                // Action signal for registering a primary
                glib::subclass::Signal::builder("register-subtimeline-primary")
                    .param_types([String::static_type(), ges::Timeline::static_type()])
                    .return_type::<Option<glib::Error>>()
                    .action()
                    .class_handler(|args| {
                        let element = args[0].get::<super::SubtimelinePrimaryManager>().unwrap();
                        let primary_id = args[1].get::<String>().unwrap();
                        let timeline = args[2].get::<ges::Timeline>().unwrap();

                        Some(
                            element
                                .imp()
                                .register_primary_internal(&primary_id, &timeline)
                                .err()
                                .to_value(),
                        )
                    })
                    .build(),
                // Signal to notify users that a new primary has been registered
                glib::subclass::Signal::builder("subtimeline-primary-registered")
                    .param_types([String::static_type(), ges::Timeline::static_type()])
                    .build(),
                // Action signal for unregistering a primary
                glib::subclass::Signal::builder("unregister-subtimeline-primary")
                    .param_types([String::static_type()])
                    .return_type::<Option<glib::Error>>()
                    .action()
                    .class_handler(|args| {
                        let element = args[0].get::<super::SubtimelinePrimaryManager>().unwrap();
                        let primary_id = args[1].get::<String>().unwrap();

                        Some(
                            element
                                .imp()
                                .unregister_primary_internal(&primary_id)
                                .to_value(),
                        )
                    })
                    .build(),
                // Normal signal emitted when primary is unregistered
                glib::subclass::Signal::builder("subtimeline-primary-unregistered")
                    .param_types([String::static_type(), ges::Timeline::static_type()])
                    .build(),
                // Signal emitted when a new timeline replica is created
                glib::subclass::Signal::builder("new-timeline-replica")
                    .param_types([
                        String::static_type(),
                        ges::Timeline::static_type(),
                        ges::Timeline::static_type(),
                    ])
                    .build(),
                // Action signal for getting primary timeline from ID
                glib::subclass::Signal::builder("get-primary")
                    .param_types([String::static_type()])
                    .return_type::<Option<ges::Timeline>>()
                    .action()
                    .class_handler(|args| {
                        let element = args[0].get::<super::SubtimelinePrimaryManager>().unwrap();
                        let primary_id = args[1].get::<String>().unwrap();

                        Some(element.imp().primary_timeline(&primary_id).to_value())
                    })
                    .build(),
            ]
        })
    }
}

impl GstObjectImpl for SubtimelinePrimaryManager {}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            // Prevent loading rust formatters from system to avoid duplicate type registration
            gst::init().unwrap();
            crate::plugin_register_static().expect("Failed to register rsges plugin");
            ges::init().unwrap();
        });
    }

    fn setup_primary_and_replica(
        test_name: &str,
        setup_primary_timeline: Option<fn(&ges::Timeline, &ges::Layer)>,
    ) -> (
        super::super::SubtimelinePrimaryManager,
        ges::Timeline, // primary
        ges::Timeline, // replica
        ges::Layer,    // primary layer
    ) {
        let manager = super::super::SubtimelinePrimaryManager::get();
        let primary = ges::Timeline::new();

        // Add track to primary
        let track = ges::VideoTrack::new();
        primary.add_track(&track).unwrap();

        // Add layer
        let layer = primary.append_layer();

        setup_primary_timeline.map(|f| f(&primary, &layer));

        // Register primary and create replica
        manager
            .downcast_ref::<super::super::SubtimelinePrimaryManager>()
            .unwrap()
            .imp()
            .register_primary(test_name, &primary)
            .unwrap();
        let replica = ges::Timeline::new();
        manager
            .downcast_ref::<super::super::SubtimelinePrimaryManager>()
            .unwrap()
            .imp()
            .make_replica(test_name, &replica)
            .unwrap();

        (manager, primary, replica, layer)
    }

    #[test]
    fn test_primary_registration() {
        init();

        let manager = super::super::SubtimelinePrimaryManager::get();
        let timeline = ges::Timeline::new();

        // Test successful registration
        let result = manager.imp().register_primary("test_primary", &timeline);
        assert!(result.is_ok(), "Should successfully register primary");

        // Test duplicate registration fails
        let result = manager.imp().register_primary("test_primary", &timeline);
        assert!(result.is_err(), "Should fail to register duplicate primary");

        // Test retrieval
        let retrieved = manager.imp().primary_timeline("test_primary");
        assert!(retrieved.is_some(), "Should retrieve registered primary");

        // Test unregistration
        let result = manager.imp().unregister_primary("test_primary");
        assert!(result.is_ok(), "Should successfully unregister primary");

        // Test retrieval after unregistration
        let retrieved = manager.imp().primary_timeline("test_primary");
        assert!(
            retrieved.is_none(),
            "Should not retrieve unregistered primary"
        );
    }

    #[test]
    fn test_replica_creation() {
        init();

        let manager = super::super::SubtimelinePrimaryManager::get();
        let primary = ges::Timeline::new();

        // Add track to primary
        let track = ges::VideoTrack::new();
        primary.add_track(&track).unwrap();

        // Add layer to primary
        let _layer = primary.append_layer();

        // Register primary
        manager
            .imp()
            .register_primary("test_primary", &primary)
            .unwrap();

        // Create replica timeline and make it a replica
        let replica = ges::Timeline::new();
        let result = manager.imp().make_replica("test_primary", &replica);
        assert!(
            result.is_ok(),
            "Should successfully make replica from primary"
        );

        // Verify replica structure
        assert_eq!(
            replica.tracks().len(),
            primary.tracks().len(),
            "Instance should have same number of tracks"
        );

        assert_eq!(
            replica.layers().len(),
            primary.layers().len(),
            "Instance should have same number of layers"
        );

        // Clean up
        manager.imp().unregister_primary("test_primary").unwrap();
    }

    #[test]
    fn test_replica_editing_disabled() {
        init();

        let manager = super::super::SubtimelinePrimaryManager::get();
        let primary = ges::Timeline::new();

        // Set properties on primary (these should NOT be copied)
        primary.set_auto_transition(true);
        primary.set_snapping_distance(10 * gst::ClockTime::MSECOND);

        // Register and create replica
        manager
            .imp()
            .register_primary("test_primary", &primary)
            .unwrap();
        let replica = ges::Timeline::new();
        manager
            .imp()
            .make_replica("test_primary", &replica)
            .unwrap();

        // Verify editing features are disabled on replica
        assert_eq!(
            replica.is_auto_transition(),
            false,
            "Auto-transition should be disabled on replica"
        );

        // Clean up
        manager.imp().unregister_primary("test_primary").unwrap();
    }

    #[test]
    fn test_layer_with_clips() {
        init();

        let manager = super::super::SubtimelinePrimaryManager::get();
        let primary = ges::Timeline::new();

        // Add track
        let track = ges::VideoTrack::new();
        primary.add_track(&track).unwrap();

        // Add layer with clip
        let layer = primary.append_layer();

        if let Some(clip) = ges::TestClip::new() {
            clip.set_start(gst::ClockTime::ZERO);
            clip.set_duration(5 * gst::ClockTime::SECOND);
            layer.add_clip(&clip).unwrap();
        }

        // Register and create replica
        manager
            .imp()
            .register_primary("test_primary", &primary)
            .unwrap();
        let replica = ges::Timeline::new();
        manager
            .imp()
            .make_replica("test_primary", &replica)
            .unwrap();

        // Verify clip was copied
        let replica_layers = replica.layers();
        assert_eq!(replica_layers.len(), 1, "Should have one layer");

        let replica_layer = &replica_layers[0];
        let clips = replica_layer.clips();

        let replica_clip = &clips[0];
        assert_eq!(
            replica_clip.duration(),
            5 * gst::ClockTime::SECOND,
            "Clip duration should be preserved"
        );

        manager.imp().unregister_primary("test_primary").unwrap();
    }

    fn check_child_property(element: &ges::Clip, property_name: &str, expected: f64) {
        let value = element
            .child_property(property_name)
            .unwrap()
            .get::<f64>()
            .unwrap();
        assert_eq!(
            value, expected,
            "Child property '{}' should be {}",
            property_name, expected
        );
    }

    #[test]
    fn test_child_property_synchronization() {
        init();

        let (manager, _primary_timeline, replica_timeline, primary_layer) =
            setup_primary_and_replica(
                "test_child_props",
                Some(|_timeline, layer| {
                    // Add clip - track elements are created automatically
                    let clip = ges::TestClip::new().unwrap();
                    clip.set_start(gst::ClockTime::ZERO);
                    clip.set_duration(5 * gst::ClockTime::SECOND);
                    layer.add_clip(&clip).unwrap();
                    // Test: Set child property on primary clip
                    clip.set_child_property("alpha", 0.5f64).unwrap();
                }),
            );

        // Get replica clip
        let replica_layers = replica_timeline.layers();
        let replica_clips = replica_layers[0].clips();
        let replica_clip = &replica_clips[0];

        // Verify property synced to replica clip (synchronous)
        check_child_property(replica_clip, "alpha", 0.5f64);
        primary_layer.clips()[0]
            .set_child_property("alpha", 1.0)
            .unwrap();

        check_child_property(replica_clip, "alpha", 1.0f64);

        manager
            .imp()
            .unregister_primary("test_child_props")
            .unwrap();
    }

    #[test]
    fn test_dynamically_added_clip_child_properties() {
        init();

        let (manager, _primary, replica, layer) = setup_primary_and_replica("test_dynamic", None);

        // Add a clip AFTER replication
        let asset = ges::Asset::request::<ges::TestClip>(None).unwrap();
        let clip = layer
            .add_asset(
                &asset,
                gst::ClockTime::ZERO,
                gst::ClockTime::ZERO,
                5 * gst::ClockTime::SECOND,
                ges::TrackType::UNKNOWN,
            )
            .unwrap();

        // Get replica clip
        let replica_layers = replica.layers();
        let replica_clips = replica_layers[0].clips();
        let replica_clip = &replica_clips[0];

        // Set child property on primary clip
        clip.set_child_property("alpha", 0.3f64).unwrap();

        // Verify property synced (synchronous)
        let replica_alpha = replica_clip
            .child_property("alpha")
            .unwrap()
            .get::<f64>()
            .unwrap();

        assert_eq!(
            replica_alpha, 0.3f64,
            "Dynamically added clip's child properties should sync"
        );

        // Clean up
        manager.imp().unregister_primary("test_dynamic").unwrap();
    }

    #[test]
    fn test_control_binding_synchronization() {
        init();

        let (manager, _primary, replica, _layer) = setup_primary_and_replica(
            "test_control_binding",
            Some(|_timeline, layer| {
                let asset = ges::Asset::request::<ges::TestClip>(None).unwrap();
                let clip = layer
                    .add_asset(
                        &asset,
                        gst::ClockTime::ZERO,
                        gst::ClockTime::ZERO,
                        5 * gst::ClockTime::SECOND,
                        ges::TrackType::UNKNOWN,
                    )
                    .unwrap();

                // Set control binding BEFORE replica is created
                let track_elements = clip.children(false);
                let track_element = track_elements[0]
                    .downcast_ref::<ges::TrackElement>()
                    .unwrap();

                let control_source = gst_controller::InterpolationControlSource::new();
                control_source.set(gst::ClockTime::ZERO, 0.0);
                control_source.set(2 * gst::ClockTime::SECOND, 0.5);
                control_source.set(4 * gst::ClockTime::SECOND, 1.0);

                track_element.set_control_source(&control_source, "alpha", "direct");
            }),
        );

        let property_name = "alpha";

        // Get replica track element
        let replica_layers = replica.layers();
        let replica_clips = replica_layers[0].clips();
        let replica_clip = &replica_clips[0];
        let replica_track_elements = replica_clip.children(false);
        let replica_track_element = replica_track_elements[0]
            .downcast_ref::<ges::TrackElement>()
            .unwrap();

        // Verify the control binding was replicated
        let replica_binding = replica_track_element.control_binding(&property_name);
        assert!(
            replica_binding.is_some(),
            "Control binding should be replicated to replica track element"
        );

        // Verify the control source has the same keyframes
        let replica_direct_binding = replica_binding
            .unwrap()
            .downcast::<gst_controller::DirectControlBinding>()
            .unwrap();
        let replica_control_source = replica_direct_binding
            .control_source()
            .unwrap()
            .downcast::<gst_controller::InterpolationControlSource>()
            .unwrap();

        // Check the count of keyframes (should be 3)
        assert_eq!(
            replica_control_source.count(),
            3,
            "Replica should have the same number of keyframes"
        );

        // Clean up
        manager
            .imp()
            .unregister_primary("test_control_binding")
            .unwrap();
    }

    #[test]
    fn test_dynamic_keyframe_synchronization() {
        init();

        let (manager, _primary, replica, _layer) = setup_primary_and_replica(
            "test_dynamic_keyframes",
            Some(|_timeline, layer| {
                // Add clip with track elements
                let clip = ges::TestClip::new().unwrap();
                clip.set_start(gst::ClockTime::ZERO);
                clip.set_duration(10 * gst::ClockTime::SECOND);
                layer.add_clip(&clip).unwrap();

                // Set control binding BEFORE replica is created
                let track_elements = clip.children(false);
                let track_element = track_elements[0]
                    .downcast_ref::<ges::TrackElement>()
                    .unwrap();

                let control_source = gst_controller::InterpolationControlSource::new();
                control_source.set(gst::ClockTime::ZERO, 0.0);
                control_source.set(5 * gst::ClockTime::SECOND, 0.5);

                track_element.set_control_source(&control_source, "alpha", "direct");
            }),
        );

        let property_name = "alpha";

        // Get replica control source
        let replica_layers = replica.layers();
        let replica_clips = replica_layers[0].clips();
        let replica_clip = &replica_clips[0];
        let replica_track_elements = replica_clip.children(false);
        let replica_track_element = replica_track_elements[0]
            .downcast_ref::<ges::TrackElement>()
            .unwrap();

        let replica_binding = replica_track_element
            .control_binding(&property_name)
            .unwrap();
        let replica_direct_binding = replica_binding
            .downcast::<gst_controller::DirectControlBinding>()
            .unwrap();
        let replica_control_source = replica_direct_binding
            .control_source()
            .unwrap()
            .downcast::<gst_controller::InterpolationControlSource>()
            .unwrap();

        // Verify initial keyframes (should be 2)
        assert_eq!(
            replica_control_source.count(),
            2,
            "Replica should initially have 2 keyframes"
        );

        // Clean up
        manager
            .imp()
            .unregister_primary("test_dynamic_keyframes")
            .unwrap();
    }

    #[test]
    fn test_dynamic_control_binding_changes() {
        init();

        let (manager, _primary, replica, layer) = setup_primary_and_replica(
            "test_dynamic_control_binding_changes",
            Some(|_timeline, layer| {
                // Add clip with track elements
                let clip = ges::TestClip::new().unwrap();
                clip.set_start(gst::ClockTime::ZERO);
                clip.set_duration(10 * gst::ClockTime::SECOND);
                layer.add_clip(&clip).unwrap();

                // Initially NO control binding - just property binding
                // Set a property value so we can verify property binding is working
                clip.set_child_property("alpha", 0.5f64).unwrap();
            }),
        );

        let property_name = "alpha";

        // Get primary track element
        let primary_clip = layer.clips()[0].clone();
        let primary_track_elements = primary_clip.children(false);
        let primary_track_element = primary_track_elements[0]
            .downcast_ref::<ges::TrackElement>()
            .unwrap();

        // Get replica track element
        let replica_layers = replica.layers();
        let replica_clips = replica_layers[0].clips();
        let replica_clip = &replica_clips[0];
        let replica_track_elements = replica_clip.children(false);
        let replica_track_element = replica_track_elements[0]
            .downcast_ref::<ges::TrackElement>()
            .unwrap();

        // Verify initial state: no control binding on replica, property binding working
        assert!(
            replica_track_element
                .control_binding(&property_name)
                .is_none(),
            "Replica should not have control binding initially"
        );
        assert_eq!(
            replica_clip
                .child_property("alpha")
                .unwrap()
                .get::<f64>()
                .unwrap(),
            0.5f64,
            "Property binding should be syncing alpha value"
        );

        // Now add a control binding to the primary track element
        let control_source = gst_controller::InterpolationControlSource::new();
        control_source.set(gst::ClockTime::ZERO, 0.0);
        control_source.set(5 * gst::ClockTime::SECOND, 1.0);

        primary_track_element.set_control_source(&control_source, property_name, "direct");

        // Verify control binding was added to replica
        let replica_binding = replica_track_element.control_binding(&property_name);
        assert!(
            replica_binding.is_some(),
            "Control binding should be added to replica when added to primary"
        );

        // Verify the control source has the correct keyframes
        let replica_direct_binding = replica_binding
            .unwrap()
            .downcast::<gst_controller::DirectControlBinding>()
            .unwrap();
        let replica_control_source = replica_direct_binding
            .control_source()
            .unwrap()
            .downcast::<gst_controller::InterpolationControlSource>()
            .unwrap();

        assert_eq!(
            replica_control_source.count(),
            2,
            "Replica control source should have 2 keyframes"
        );

        // Now remove the control binding from the primary
        let _primary_binding = primary_track_element
            .control_binding(&property_name)
            .unwrap();
        primary_track_element
            .remove_control_binding(&property_name)
            .unwrap();

        // Verify control binding was removed from replica
        assert!(
            replica_track_element
                .control_binding(&property_name)
                .is_none(),
            "Control binding should be removed from replica when removed from primary"
        );

        // Verify property binding is restored by changing the primary value
        primary_clip.set_child_property("alpha", 0.75).unwrap();

        // Give a moment for property binding to sync (it's asynchronous through GObject bindings)
        assert_eq!(
            replica_clip
                .child_property("alpha")
                .unwrap()
                .get::<f64>()
                .unwrap(),
            0.75f64,
            "Property binding should be restored and syncing alpha value after control binding removal"
        );

        // Clean up
        manager
            .imp()
            .unregister_primary("test_dynamic_control_binding_changes")
            .unwrap();
    }

    #[test]
    fn test_clip_move_between_layers() {
        init();

        let (manager, primary, replica, _layer) = setup_primary_and_replica(
            "test_clip_layer_move",
            Some(|timeline, layer| {
                // Add a second layer
                timeline.append_layer();

                // Add a clip to the first layer
                let clip = ges::TestClip::new().unwrap();
                clip.set_start(gst::ClockTime::ZERO);
                clip.set_duration(5 * gst::ClockTime::SECOND);
                layer.add_clip(&clip).unwrap();
            }),
        );

        // Verify setup: clip should be on first layer in both primary and replica
        let primary_layers = primary.layers();
        assert_eq!(primary_layers.len(), 2, "Primary should have 2 layers");

        let primary_clip = primary_layers[0].clips()[0].clone();
        assert_eq!(
            primary_clip.layer().unwrap().priority(),
            primary_layers[0].priority(),
            "Primary clip should be on layer 0"
        );

        let replica_layers = replica.layers();
        assert_eq!(replica_layers.len(), 2, "Replica should have 2 layers");

        let replica_clip = replica_layers[0].clips()[0].clone();
        assert_eq!(
            replica_clip.layer().unwrap().priority(),
            replica_layers[0].priority(),
            "Replica clip should be on layer 0"
        );

        // Move the clip to the second layer on the primary
        primary_clip.move_to_layer(&primary_layers[1]).unwrap();

        // Verify the clip moved on the primary
        assert_eq!(
            primary_clip.layer().unwrap().priority(),
            primary_layers[1].priority(),
            "Primary clip should now be on layer 1"
        );

        // Verify the clip also moved on the replica
        assert_eq!(
            replica_clip.layer().unwrap().priority(),
            replica_layers[1].priority(),
            "Replica clip should also have moved to layer 1"
        );

        // Verify clip is no longer on the first layer's clips list
        assert_eq!(
            replica_layers[0].clips().len(),
            0,
            "First replica layer should have no clips"
        );
        assert_eq!(
            replica_layers[1].clips().len(),
            1,
            "Second replica layer should have the clip"
        );

        // Clean up
        manager
            .imp()
            .unregister_primary("test_clip_layer_move")
            .unwrap();
    }
}
