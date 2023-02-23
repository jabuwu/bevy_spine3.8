//! There's not much documentation yet. Check out
//! [the examples](https://github.com/jabuwu/bevy_spine/tree/main/examples) and the
//! [rusty_spine docs](https://docs.rs/rusty_spine/0.5.0)

use std::{
    collections::{HashMap, VecDeque},
    f32::EPSILON,
    mem::take,
    sync::{Arc, Mutex},
};

use bevy::{
    prelude::*,
    render::{
        mesh::{Indices, MeshVertexAttribute},
        render_resource::{PrimitiveTopology, VertexFormat},
    },
    sprite::{Material2dPlugin, Mesh2dHandle},
};
use materials::{
    SpineAdditiveMaterial, SpineAdditivePmaMaterial, SpineMultiplyMaterial,
    SpineMultiplyPmaMaterial, SpineNormalMaterial, SpineNormalPmaMaterial, SpineScreenMaterial,
    SpineScreenPmaMaterial, SpineShader,
};
use rusty_spine::{BlendMode, Skeleton};

use crate::{
    assets::{AtlasLoader, SkeletonJsonLoader},
    rusty_spine::{
        controller::SkeletonControllerSettings, draw::CullDirection, AnimationStateData,
        BoneHandle, EventType,
    },
    textures::{SpineTexture, SpineTextureCreateEvent, SpineTextureDisposeEvent, SpineTextures},
};

pub use crate::{
    assets::*,
    crossfades::Crossfades,
    entity_sync::*,
    rusty_spine::{controller::SkeletonController, Color},
};

pub use rusty_spine;

/// System sets for Spine systems.
#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy, SystemSet)]
pub enum SpineSystem {
    /// Loads [`SkeletonData`] assets which must exist before a [`SpineBundle`] can fully load.
    Load,
    /// Spawns helper entities associated with a [`SpineBundle`] for drawing meshes and
    /// (optionally) adding bone entities (see [`SpineLoader`]).
    Spawn,
    /// An [`apply_system_buffers`] to load the spine helper entities this frame.
    SpawnFlush,
    /// Sends [`SpineReadyEvent`] after [`SpineSet::SpawnFlush`], indicating [`Spine`] components on
    /// newly spawned [`SpineBundle`]s can now be interacted with.
    Ready,
    /// Advances all animations and processes Spine events (see [`SpineEvent`]).
    Update,
    /// Updates all Spine meshes and materials for rendering.
    Render,
}

/// Helper sets for interacting with Spine systems.
#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy, SystemSet)]
pub enum SpineSet {
    /// A helper Set which systems can be added into, occuring after [`SpineSet::Ready`] but before
    /// [`SpineSet::Update`], so that entities can configure a newly spawned skeleton before they
    /// are updated for the first time.
    OnReady,
}

/// Add Spine support to Bevy!
///
/// ```
/// # use bevy::prelude::*;
/// # use bevy_spine::SpinePlugin;
/// # fn doc() {
/// App::new()
///     .add_plugins(DefaultPlugins)
///     .add_plugin(SpinePlugin)
///     // ...
///     .run();
/// # }
/// ```
pub struct SpinePlugin;

impl Plugin for SpinePlugin {
    fn build(&self, app: &mut App) {
        {
            let mut shaders = app.world.resource_mut::<Assets<Shader>>();
            SpineShader::set(
                shaders.add(Shader::from_wgsl(include_str!("./vertex.wgsl"))),
                shaders.add(Shader::from_wgsl(include_str!("./fragment.wgsl"))),
                shaders.add(Shader::from_wgsl(include_str!("./fragment_pma.wgsl"))),
            );
        }
        app.add_plugin(Material2dPlugin::<SpineNormalMaterial>::default())
            .add_plugin(Material2dPlugin::<SpineAdditiveMaterial>::default())
            .add_plugin(Material2dPlugin::<SpineMultiplyMaterial>::default())
            .add_plugin(Material2dPlugin::<SpineScreenMaterial>::default())
            .add_plugin(Material2dPlugin::<SpineNormalPmaMaterial>::default())
            .add_plugin(Material2dPlugin::<SpineAdditivePmaMaterial>::default())
            .add_plugin(Material2dPlugin::<SpineMultiplyPmaMaterial>::default())
            .add_plugin(Material2dPlugin::<SpineScreenPmaMaterial>::default())
            .add_plugin(SpineSyncPlugin::first())
            .insert_resource(SpineTextures::init())
            .insert_resource(SpineReadyEvents::default())
            .add_event::<SpineTextureCreateEvent>()
            .add_event::<SpineTextureDisposeEvent>()
            .add_asset::<Atlas>()
            .add_asset::<SkeletonJson>()
            .add_asset::<SkeletonBinary>()
            .add_asset::<SkeletonData>()
            .init_asset_loader::<AtlasLoader>()
            .init_asset_loader::<SkeletonJsonLoader>()
            .init_asset_loader::<SkeletonBinaryLoader>()
            .add_event::<SpineReadyEvent>()
            .add_event::<SpineEvent>()
            .add_system(spine_load.in_set(SpineSystem::Load))
            .add_system(
                spine_spawn
                    .in_set(SpineSystem::Spawn)
                    .after(SpineSystem::Load),
            )
            .add_system(
                spine_ready
                    .in_set(SpineSystem::Ready)
                    .after(SpineSystem::Spawn)
                    .before(SpineSet::OnReady),
            )
            .add_system(
                spine_update
                    .in_set(SpineSystem::Update)
                    .after(SpineSet::OnReady),
            )
            .add_system(spine_render.in_set(SpineSystem::Render))
            .add_system(
                apply_system_buffers
                    .in_set(SpineSystem::SpawnFlush)
                    .after(SpineSystem::Spawn)
                    .before(SpineSystem::Ready),
            );
    }
}

/// A live Spine [`SkeletonController`] [`Component`], ready to be manipulated.
///
/// This component does not exist on [`SpineBundle`] initially, since Spine assets may not yet be
/// loaded when an entity is spawned. Querying for this component type guarantees that all entities
/// containing it have a Spine rig that is ready to use.
#[derive(Component)]
pub struct Spine(pub SkeletonController);

/// When loaded, a [`Spine`] entity has children entities attached to it, each containing this
/// component.
///
/// To disable creation of these child entities, see [`SpineLoader::without_children`].
///
/// The bones are not automatically synchronized, but can be synchronized easily by adding a
/// [`SpineSync`] component.
#[derive(Component)]
pub struct SpineBone {
    pub spine_entity: Entity,
    pub handle: BoneHandle,
    pub name: String,
}

#[derive(Component)]
pub struct SpineMesh;

impl core::ops::Deref for Spine {
    type Target = SkeletonController;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for Spine {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// The async loader for Spine assets. Waits for Spine assets to be ready in the [`AssetServer`],
/// then initializes child entities, and finally attaches the live [`Spine`] component.
///
/// When spawning a [`SpineLoader`] (typically through [`SpineBundle`]), it will create child
/// entities representing the bones of a skeleton (see [`SpineBone`]). These bones are not
/// synchronized (see [`SpineSync`]), and can be disabled entirely using
/// [`SpineLoader::without_children`].
#[derive(Component)]
pub enum SpineLoader {
    /// The spine rig is still loading.
    Loading {
        /// If true, will spawn child entities for each bone in the skeleton (see [`SpineBone`]).
        with_children: bool,
    },
    /// The spine rig is ready.
    Ready,
    /// The spine rig failed to load.
    Failed,
}

impl Default for SpineLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl SpineLoader {
    pub fn new() -> Self {
        Self::with_children()
    }

    pub fn with_children() -> Self {
        Self::Loading {
            with_children: true,
        }
    }

    /// Load a [`Spine`] entity without child entities containing [`SpineBone`] components.
    ///
    /// Renderable mesh child entities are still created.
    ///
    /// ```
    /// # use bevy::prelude::*;
    /// # use bevy_spine::{SpineLoader, SpineBundle};
    /// # fn doc(mut commands: Commands) {
    /// commands.spawn(SpineBundle {
    ///     // ..
    ///     loader: SpineLoader::without_children(),
    ///     ..Default::default()
    /// });
    /// # }
    /// ```
    pub fn without_children() -> Self {
        Self::Loading {
            with_children: false,
        }
    }
}

#[derive(Default, Bundle)]
pub struct SpineBundle {
    pub loader: SpineLoader,
    pub skeleton: Handle<SkeletonData>,
    pub crossfades: Crossfades,
    pub transform: Transform,
    pub global_transform: GlobalTransform,
    pub visibility: Visibility,
    pub computed_visibility: ComputedVisibility,
}

/// An [`Event`] which is sent once a [`SpineLoader`] has fully loaded a skeleton and attached the
/// [`Spine`] component.
///
/// For convenience, systems receiving this event can be added to the [`SpineSet::OnReady`] set to
/// receive this after events are sent, but before the first [`SkeletonController`] update.
#[derive(Debug, Clone)]
pub struct SpineReadyEvent {
    /// The entity containing the [`Spine`] component.
    pub entity: Entity,
    /// A list of all bones (if spawned, see [`SpineBone`]).
    pub bones: HashMap<String, Entity>,
}

#[derive(Debug, Clone)]
pub enum SpineEvent {
    Start {
        entity: Entity,
        animation: String,
    },
    Interrupt {
        entity: Entity,
        animation: String,
    },
    End {
        entity: Entity,
        animation: String,
    },
    Complete {
        entity: Entity,
        animation: String,
    },
    Dispose {
        entity: Entity,
    },
    Event {
        entity: Entity,
        name: String,
        int: i32,
        float: f32,
        string: String,
        audio_path: String,
        volume: f32,
        balance: f32,
    },
}

/// Queued ready events, to be sent after [`SpineSet::SpawnFlush`].
#[derive(Default, Resource)]
struct SpineReadyEvents(Vec<SpineReadyEvent>);

#[allow(clippy::too_many_arguments)]
fn spine_load(
    mut skeleton_data_assets: ResMut<Assets<SkeletonData>>,
    mut images: ResMut<Assets<Image>>,
    mut texture_create_events: EventWriter<SpineTextureCreateEvent>,
    mut texture_dispose_events: EventWriter<SpineTextureDisposeEvent>,
    atlases: Res<Assets<Atlas>>,
    jsons: Res<Assets<SkeletonJson>>,
    binaries: Res<Assets<SkeletonBinary>>,
    spine_textures: Res<SpineTextures>,
    asset_server: Res<AssetServer>,
) {
    for (_, skeleton_data_asset) in skeleton_data_assets.iter_mut() {
        let SkeletonData {
            atlas_handle,
            kind,
            status,
            premultiplied_alpha,
        } = skeleton_data_asset;
        if matches!(status, SkeletonDataStatus::Loading) {
            let atlas = if let Some(atlas) = atlases.get(atlas_handle) {
                atlas
            } else {
                continue;
            };
            if let Some(page) = atlas.atlas.pages().next() {
                *premultiplied_alpha = page.pma();
            }
            match kind {
                SkeletonDataKind::JsonFile(json_handle) => {
                    let json = if let Some(json) = jsons.get(json_handle) {
                        json
                    } else {
                        continue;
                    };
                    let skeleton_json = rusty_spine::SkeletonJson::new(atlas.atlas.clone());
                    match skeleton_json.read_skeleton_data(&json.json) {
                        Ok(skeleton_data) => {
                            *status = SkeletonDataStatus::Loaded(Arc::new(skeleton_data));
                        }
                        Err(_err) => {
                            *status = SkeletonDataStatus::Failed;
                            continue;
                        }
                    }
                }
                SkeletonDataKind::BinaryFile(binary_handle) => {
                    let binary = if let Some(binary) = binaries.get(binary_handle) {
                        binary
                    } else {
                        continue;
                    };
                    let skeleton_binary = rusty_spine::SkeletonBinary::new(atlas.atlas.clone());
                    match skeleton_binary.read_skeleton_data(&binary.binary) {
                        Ok(skeleton_data) => {
                            *status = SkeletonDataStatus::Loaded(Arc::new(skeleton_data));
                        }
                        Err(_err) => {
                            // TODO: print error?
                            *status = SkeletonDataStatus::Failed;
                            continue;
                        }
                    }
                }
            }
        }
    }

    spine_textures.update(
        asset_server.as_ref(),
        images.as_mut(),
        &mut texture_create_events,
        &mut texture_dispose_events,
    );
}

#[allow(clippy::too_many_arguments)]
fn spine_spawn(
    mut skeleton_query: Query<(
        &mut SpineLoader,
        Entity,
        &Handle<SkeletonData>,
        Option<&Crossfades>,
    )>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut ready_events: ResMut<SpineReadyEvents>,
    mut skeleton_data_assets: ResMut<Assets<SkeletonData>>,
    mut images: ResMut<Assets<Image>>,
    mut texture_create_events: EventWriter<SpineTextureCreateEvent>,
    mut texture_dispose_events: EventWriter<SpineTextureDisposeEvent>,
    spine_textures: Res<SpineTextures>,
    asset_server: Res<AssetServer>,
) {
    for (mut spine_loader, entity, data_handle, crossfades) in skeleton_query.iter_mut() {
        if let SpineLoader::Loading { with_children } = spine_loader.as_ref() {
            let skeleton_data_asset =
                if let Some(skeleton_data_asset) = skeleton_data_assets.get_mut(data_handle) {
                    skeleton_data_asset
                } else {
                    continue;
                };
            match &skeleton_data_asset.status {
                SkeletonDataStatus::Loaded(skeleton_data) => {
                    let mut animation_state_data = AnimationStateData::new(skeleton_data.clone());
                    if let Some(crossfades) = crossfades {
                        crossfades.apply(&mut animation_state_data);
                    }
                    let mut controller = SkeletonController::new(
                        skeleton_data.clone(),
                        Arc::new(animation_state_data),
                    )
                    .with_settings(
                        SkeletonControllerSettings::new()
                            .with_cull_direction(CullDirection::CounterClockwise)
                            .with_premultiplied_alpha(skeleton_data_asset.premultiplied_alpha),
                    );
                    controller.skeleton.set_to_setup_pose();
                    let mut bones = HashMap::new();
                    if let Some(mut entity_commands) = commands.get_entity(entity) {
                        entity_commands
                            .with_children(|parent| {
                                // TODO: currently, a mesh is created for each slot, however since we use the
                                // combined drawer, this many meshes is usually not necessary. instead, we
                                // may want to dynamically create meshes as needed in the render system
                                let mut z = 0.;
                                for _ in controller.skeleton.slots() {
                                    let mut mesh = Mesh::new(PrimitiveTopology::TriangleList);
                                    empty_mesh(&mut mesh);
                                    let mesh_handle = meshes.add(mesh);
                                    parent.spawn((
                                        SpineMesh,
                                        Mesh2dHandle(mesh_handle.clone()),
                                        Transform::from_xyz(0., 0., z),
                                        GlobalTransform::default(),
                                        Visibility::default(),
                                        ComputedVisibility::default(),
                                    ));
                                    z += EPSILON;
                                }
                                if *with_children {
                                    spawn_bones(
                                        entity,
                                        parent,
                                        &controller.skeleton,
                                        controller.skeleton.bone_root().handle(),
                                        &mut bones,
                                    );
                                }
                            })
                            .insert(Spine(controller));
                    }
                    *spine_loader = SpineLoader::Ready;
                    ready_events.0.push(SpineReadyEvent { entity, bones });
                }
                SkeletonDataStatus::Loading => {}
                SkeletonDataStatus::Failed => {
                    *spine_loader = SpineLoader::Failed;
                }
            }
        }
    }

    spine_textures.update(
        asset_server.as_ref(),
        images.as_mut(),
        &mut texture_create_events,
        &mut texture_dispose_events,
    );
}

fn spawn_bones(
    spine_entity: Entity,
    parent: &mut ChildBuilder,
    skeleton: &Skeleton,
    bone: BoneHandle,
    bones: &mut HashMap<String, Entity>,
) {
    if let Some(bone) = bone.get(skeleton) {
        let mut transform = Transform::default();
        transform.translation.x = bone.applied_x();
        transform.translation.y = bone.applied_y();
        transform.translation.z = 0.;
        transform.rotation = Quat::from_axis_angle(Vec3::Z, bone.applied_rotation().to_radians());
        transform.scale.x = bone.applied_scale_x();
        transform.scale.y = bone.applied_scale_y();
        let bone_entity = parent
            .spawn((
                transform,
                GlobalTransform::default(),
                Visibility::default(),
                ComputedVisibility::default(),
            ))
            .insert(SpineBone {
                spine_entity,
                handle: bone.handle(),
                name: bone.data().name().to_owned(),
            })
            .with_children(|parent| {
                for child in bone.children() {
                    spawn_bones(spine_entity, parent, skeleton, child.handle(), bones);
                }
            })
            .id();
        bones.insert(bone.data().name().to_owned(), bone_entity);
    }
}

fn spine_ready(
    mut ready_events: ResMut<SpineReadyEvents>,
    mut ready_writer: EventWriter<SpineReadyEvent>,
) {
    for event in take(&mut ready_events.0).into_iter() {
        ready_writer.send(event);
    }
}

#[derive(Default)]
struct SpineUpdateLocal {
    events: Arc<Mutex<VecDeque<SpineEvent>>>,
}

fn spine_update(
    mut spine_query: Query<(Entity, &mut Spine)>,
    mut spine_ready_events: EventReader<SpineReadyEvent>,
    mut spine_events: EventWriter<SpineEvent>,
    time: Res<Time>,
    local: Local<SpineUpdateLocal>,
) {
    for event in spine_ready_events.iter() {
        if let Ok((entity, mut spine)) = spine_query.get_mut(event.entity) {
            let events = local.events.clone();
            spine.animation_state.set_listener(
                move |_animation_state, event_type, track_entry, spine_event| match event_type {
                    EventType::Start => {
                        let mut events = events.lock().unwrap();
                        events.push_back(SpineEvent::Start {
                            entity,
                            animation: track_entry.animation().name().to_owned(),
                        });
                    }
                    EventType::Interrupt => {
                        let mut events = events.lock().unwrap();
                        events.push_back(SpineEvent::Interrupt {
                            entity,
                            animation: track_entry.animation().name().to_owned(),
                        });
                    }
                    EventType::End => {
                        let mut events = events.lock().unwrap();
                        events.push_back(SpineEvent::End {
                            entity,
                            animation: track_entry.animation().name().to_owned(),
                        });
                    }
                    EventType::Complete => {
                        let mut events = events.lock().unwrap();
                        events.push_back(SpineEvent::Complete {
                            entity,
                            animation: track_entry.animation().name().to_owned(),
                        });
                    }
                    EventType::Dispose => {
                        let mut events = events.lock().unwrap();
                        events.push_back(SpineEvent::Dispose { entity });
                    }
                    EventType::Event => {
                        if let Some(spine_event) = spine_event {
                            let mut events = events.lock().unwrap();
                            events.push_back(SpineEvent::Event {
                                entity,
                                name: spine_event.data().name().to_owned(),
                                int: spine_event.int_value(),
                                float: spine_event.float_value(),
                                string: spine_event.string_value().to_owned(),
                                audio_path: spine_event.data().audio_path().to_owned(),
                                volume: spine_event.volume(),
                                balance: spine_event.balance(),
                            });
                        }
                    }
                    _ => {}
                },
            );
        }
    }
    for (_, mut spine) in spine_query.iter_mut() {
        spine.update(time.delta_seconds());
    }
    {
        let mut events = local.events.lock().unwrap();
        while let Some(event) = events.pop_front() {
            spine_events.send(event);
        }
    }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn spine_render(
    mut commands: Commands,
    mut spine_query: Query<(&mut Spine, &Children)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut normal_materials: ResMut<Assets<SpineNormalMaterial>>,
    mut additive_materials: ResMut<Assets<SpineAdditiveMaterial>>,
    mut multiply_materials: ResMut<Assets<SpineMultiplyMaterial>>,
    mut screen_materials: ResMut<Assets<SpineScreenMaterial>>,
    mut normal_pma_materials: ResMut<Assets<SpineNormalPmaMaterial>>,
    mut additive_pma_materials: ResMut<Assets<SpineAdditivePmaMaterial>>,
    mut multiply_pma_materials: ResMut<Assets<SpineMultiplyPmaMaterial>>,
    mut screen_pma_materials: ResMut<Assets<SpineScreenPmaMaterial>>,
    mesh_query: Query<
        (
            Entity,
            &Mesh2dHandle,
            Option<&Handle<SpineNormalMaterial>>,
            Option<&Handle<SpineAdditiveMaterial>>,
            Option<&Handle<SpineMultiplyMaterial>>,
            Option<&Handle<SpineScreenMaterial>>,
            Option<&Handle<SpineNormalPmaMaterial>>,
            Option<&Handle<SpineAdditivePmaMaterial>>,
            Option<&Handle<SpineMultiplyPmaMaterial>>,
            Option<&Handle<SpineScreenPmaMaterial>>,
        ),
        With<SpineMesh>,
    >,
    asset_server: Res<AssetServer>,
) {
    for (mut spine, spine_children) in spine_query.iter_mut() {
        let mut renderables = spine.0.combined_renderables();
        let mut renderable_index = 0;
        for child in spine_children.iter() {
            if let Ok((
                mesh_entity,
                mesh_handle,
                normal_material_handle,
                additive_material_handle,
                multiply_material_handle,
                screen_material_handle,
                normal_pma_material_handle,
                additive_pma_material_handle,
                multiply_pma_material_handle,
                screen_pma_material_handle,
            )) = mesh_query.get(*child)
            {
                let mesh = meshes.get_mut(&mesh_handle.0).unwrap();
                if let Some(renderable) = renderables.get_mut(renderable_index) {
                    let mut normals = vec![];
                    for _ in 0..renderable.vertices.len() {
                        normals.push([0., 0., 0.]);
                    }
                    mesh.set_indices(Some(Indices::U16(take(&mut renderable.indices))));
                    mesh.insert_attribute(
                        MeshVertexAttribute::new("Vertex_Position", 0, VertexFormat::Float32x2),
                        take(&mut renderable.vertices),
                    );
                    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
                    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, take(&mut renderable.uvs));
                    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, take(&mut renderable.colors));
                    mesh.insert_attribute(
                        MeshVertexAttribute::new("Vertex_DarkColor", 5, VertexFormat::Float32x4),
                        take(&mut renderable.dark_colors),
                    );

                    macro_rules! apply_material {
                        ($condition:expr, $material:ty, $handle:ident, $assets:ident) => {
                            if let Some(attachment_render_object) =
                                renderable.attachment_renderer_object
                            {
                                let spine_texture = unsafe {
                                    &mut *(attachment_render_object as *mut SpineTexture)
                                };
                                let texture_path = spine_texture.0.clone();
                                if $condition {
                                    let handle = if let Some(handle) = $handle {
                                        handle.clone()
                                    } else {
                                        let handle = $assets.add(<$material>::new(
                                            asset_server.load(texture_path.as_str()),
                                        ));
                                        if let Some(mut entity_commands) =
                                            commands.get_entity(mesh_entity)
                                        {
                                            entity_commands.insert(handle.clone());
                                        }
                                        handle
                                    };
                                    if let Some(material) = $assets.get_mut(&handle) {
                                        material.image = asset_server.load(texture_path.as_str());
                                    }
                                } else {
                                    if $handle.is_some() {
                                        if let Some(mut entity_commands) =
                                            commands.get_entity(mesh_entity)
                                        {
                                            entity_commands.remove::<Handle<$material>>();
                                        }
                                    }
                                }
                            } else {
                                if $handle.is_some() {
                                    if let Some(mut entity_commands) =
                                        commands.get_entity(mesh_entity)
                                    {
                                        entity_commands.remove::<Handle<$material>>();
                                    }
                                }
                            }
                        };
                    }

                    apply_material!(
                        renderable.blend_mode == BlendMode::Normal
                            && !renderable.premultiplied_alpha,
                        SpineNormalMaterial,
                        normal_material_handle,
                        normal_materials
                    );
                    apply_material!(
                        renderable.blend_mode == BlendMode::Additive
                            && !renderable.premultiplied_alpha,
                        SpineAdditiveMaterial,
                        additive_material_handle,
                        additive_materials
                    );
                    apply_material!(
                        renderable.blend_mode == BlendMode::Multiply
                            && !renderable.premultiplied_alpha,
                        SpineMultiplyMaterial,
                        multiply_material_handle,
                        multiply_materials
                    );
                    apply_material!(
                        renderable.blend_mode == BlendMode::Screen
                            && !renderable.premultiplied_alpha,
                        SpineScreenMaterial,
                        screen_material_handle,
                        screen_materials
                    );
                    apply_material!(
                        renderable.blend_mode == BlendMode::Normal
                            && renderable.premultiplied_alpha,
                        SpineNormalPmaMaterial,
                        normal_pma_material_handle,
                        normal_pma_materials
                    );
                    apply_material!(
                        renderable.blend_mode == BlendMode::Additive
                            && renderable.premultiplied_alpha,
                        SpineAdditivePmaMaterial,
                        additive_pma_material_handle,
                        additive_pma_materials
                    );
                    apply_material!(
                        renderable.blend_mode == BlendMode::Multiply
                            && renderable.premultiplied_alpha,
                        SpineMultiplyPmaMaterial,
                        multiply_pma_material_handle,
                        multiply_pma_materials
                    );
                    apply_material!(
                        renderable.blend_mode == BlendMode::Screen
                            && renderable.premultiplied_alpha,
                        SpineScreenPmaMaterial,
                        screen_pma_material_handle,
                        screen_pma_materials
                    );
                } else {
                    empty_mesh(mesh);
                }
                renderable_index += 1;
            }
        }
    }
}

fn empty_mesh(mesh: &mut Mesh) {
    let indices = Indices::U32(vec![]);

    let positions: Vec<[f32; 3]> = vec![];
    let normals: Vec<[f32; 3]> = vec![];
    let uvs: Vec<[f32; 2]> = vec![];
    let colors: Vec<[f32; 4]> = vec![];
    let dark_colors: Vec<[f32; 4]> = vec![];

    mesh.set_indices(Some(indices));
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_attribute(
        MeshVertexAttribute::new("Vertex_DarkColor", 5, VertexFormat::Float32x4),
        dark_colors,
    );
}

mod assets;
mod crossfades;
mod entity_sync;

pub mod materials;
pub mod prelude;
pub mod textures;

#[cfg(test)]
mod test;
