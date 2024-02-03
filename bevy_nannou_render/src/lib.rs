use std::collections::HashMap;
use std::ops::Deref;

use bevy::asset::load_internal_asset;
use bevy::core_pipeline::core_3d;
use bevy::core_pipeline::core_3d::CORE_3D;
use bevy::prelude::*;
use bevy::render::camera::{ExtractedCamera, NormalizedRenderTarget};
use bevy::render::extract_component::ExtractComponentPlugin;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::{RenderAsset, RenderAssets};
use bevy::render::render_graph::{RenderGraphApp, ViewNode, ViewNodeRunner};
use bevy::render::render_resource::{
    CachedRenderPipelineId, PipelineCache, ShaderType, SpecializedRenderPipeline,
    SpecializedRenderPipelines,
};
use bevy::render::renderer::RenderDevice;
use bevy::render::texture::BevyDefault;
use bevy::render::view::{ExtractedView, ExtractedWindow, ExtractedWindows, ViewDepthTexture, ViewTarget, ViewUniforms};
use bevy::render::{render_resource as wgpu, RenderSet};
use bevy::render::{Render, RenderApp};
use lyon::lyon_tessellation::{FillTessellator, StrokeTessellator};

use bevy_nannou_draw::draw::render::{GlyphCache, RenderContext, RenderPrimitive, Scissor};
use bevy_nannou_draw::{draw, Draw};
use nannou_core::geom;
use nannou_core::math::map_range;

use crate::pipeline::{NannouPipeline, NannouPipelineKey, NannouViewNode, TextureBindGroupCache};

mod pipeline;

pub struct NannouRenderPlugin;

pub const NANNOU_SHADER_HANDLE: Handle<Shader> = Handle::weak_from_u128(43700360588854283521);

impl Plugin for NannouRenderPlugin {
    fn build(&self, app: &mut App) {
        load_internal_asset!(
            app,
            NANNOU_SHADER_HANDLE,
            "shaders/nannou.wgsl",
            Shader::from_wgsl
        );

        app.add_systems(Startup, setup_default_texture)
            .add_plugins(ExtractComponentPlugin::<Draw>::default())
            .add_plugins(ExtractResourcePlugin::<DefaultTextureHandle>::default());

        app.get_sub_app_mut(RenderApp)
            .unwrap()
            // TODO: how are these parameters defined? should they be configurable?
            .insert_resource(GlyphCache::new([1024; 2], 0.1, 0.1))
            .init_resource::<SpecializedRenderPipelines<NannouPipeline>>()
            .init_resource::<TextureBindGroupCache>()
            .add_systems(
                Render,
                (
                    prepare_default_texture_bind_group.in_set(RenderSet::PrepareBindGroups),
                    prepare_view_mesh.after(prepare_default_texture_bind_group),
                    prepare_view_uniform.in_set(RenderSet::PrepareBindGroups),
                ),
            )
            // Register the NannouViewNode with the render graph
            // The node runs at the last stage of the main 3d pass
            .add_render_graph_node::<ViewNodeRunner<NannouViewNode>>(
                core_3d::graph::NAME,
                NannouViewNode::NAME,
            )
            .add_render_graph_edges(
                CORE_3D,
                &[
                    core_3d::graph::node::MAIN_TRANSPARENT_PASS,
                    NannouViewNode::NAME,
                    core_3d::graph::node::END_MAIN_PASS,
                ],
            );
    }

    fn finish(&self, app: &mut App) {
        let Ok(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app.init_resource::<NannouPipeline>();
    }
}

#[derive(Resource, Deref, DerefMut, ExtractResource, Clone)]
struct DefaultTextureHandle(Handle<Image>);

fn setup_default_texture(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let texture = images.add(Image::default());
    commands.insert_resource(DefaultTextureHandle(texture));
}

fn prepare_default_texture_bind_group(
    images: Res<RenderAssets<Image>>,
    default_texture_handle: Res<DefaultTextureHandle>,
    render_device: Res<RenderDevice>,
    nannou_pipeline: Res<NannouPipeline>,
    mut texture_bind_group_cache: ResMut<TextureBindGroupCache>,
) {
    let texture = images.get(&**default_texture_handle).unwrap();
    let bind_group = NannouPipeline::create_texture_bind_group(
        &render_device,
        &nannou_pipeline.texture_bind_group_layout,
        &texture.sampler,
        &texture.texture_view,
    );
    texture_bind_group_cache.insert((**default_texture_handle).clone(), bind_group);
}

// Prepare our mesh for rendering
fn prepare_view_mesh(
    mut commands: Commands,
    mut pipeline: ResMut<NannouPipeline>,
    mut glyph_cache: ResMut<GlyphCache>,
    mut texture_bind_group_cache: ResMut<TextureBindGroupCache>,
    mut pipelines: ResMut<SpecializedRenderPipelines<NannouPipeline>>,
    windows: Res<ExtractedWindows>,
    msaa: Res<Msaa>,
    pipeline_cache: Res<PipelineCache>,
    default_texture_handle: Res<DefaultTextureHandle>,
    draw: Query<(
        Entity,
        &Draw,
        &ExtractedView,
        &ExtractedCamera,
        &ViewDepthTexture,
    )>,
) {
    for (entity, draw, view, camera, depth) in &draw {
        let mut render_commands = ViewRenderCommands::default();
        let mut mesh = ViewMesh::default();

        // Pushes a draw command and updates the `curr_start_index`.
        //
        // Returns `true` if the command was added, `false` if there was nothing to
        // draw.
        fn push_draw_cmd(
            curr_start_index: &mut u32,
            end_index: u32,
            render_commands: &mut Vec<RenderCommand>,
        ) -> bool {
            let index_range = *curr_start_index..end_index;
            if index_range.len() != 0 {
                let start_vertex = 0;
                *curr_start_index = index_range.end;
                let cmd = RenderCommand::DrawIndexed {
                    start_vertex,
                    index_range,
                };
                render_commands.push(cmd);
                true
            } else {
                false
            }
        }

        let window = if let Some(NormalizedRenderTarget::Window(window_ref)) = camera.target {
            let window_entity = window_ref.entity();
            if let Some(window) = windows.windows.get(&window_entity) {
                window
            } else {
                continue;
            }
        } else {
            // TODO: handle other render targets
            // For now, we only support rendering to a window
            continue;
        };

        // TODO: Unclear if we need to track this, or if the physical size is enough.
        let scale_factor = 1.0;
        let [w_px, h_px] = [window.physical_width, window.physical_height];

        // Converting between pixels and points.
        let px_to_pt = |s: u32| s as f32 / scale_factor;
        let pt_to_px = |s: f32| (s * scale_factor).round() as u32;
        let full_rect = nannou_core::geom::Rect::from_w_h(px_to_pt(w_px), px_to_pt(h_px));

        let window_to_scissor = |v: nannou_core::geom::Vec2| -> [u32; 2] {
            let x = map_range(v.x, full_rect.left(), full_rect.right(), 0u32, w_px);
            let y = map_range(v.y, full_rect.bottom(), full_rect.top(), 0u32, h_px);
            [x, y]
        };

        // TODO: Store these in `Renderer`.
        let mut fill_tessellator = FillTessellator::new();
        let mut stroke_tessellator = StrokeTessellator::new();

        // Keep track of context changes.
        let mut curr_ctxt = draw::Context::default();
        let mut curr_start_index = 0;
        // Track whether new commands are required.
        let mut curr_pipeline_id = None;
        let mut curr_scissor = None;
        let mut curr_texture_handle: Option<Handle<Image>> = None;

        // Collect all draw commands to avoid borrow errors.
        let draw_cmds: Vec<_> = draw.drain_commands().collect();
        let draw_state = draw.state.write().expect("failed to lock draw state");
        let intermediary_state = draw_state
            .intermediary_state
            .read()
            .expect("failed to lock intermediary state");
        for cmd in draw_cmds {
            match cmd {
                draw::DrawCommand::Context(ctxt) => curr_ctxt = ctxt,
                draw::DrawCommand::Primitive(prim) => {
                    // Track the prev index and vertex counts.
                    let prev_index_count = mesh.indices().len() as u32;
                    let prev_vert_count = mesh.vertex_count();

                    // Info required during rendering.
                    let ctxt = RenderContext {
                        intermediary_mesh: &intermediary_state.intermediary_mesh,
                        path_event_buffer: &intermediary_state.path_event_buffer,
                        path_points_colored_buffer: &intermediary_state.path_points_colored_buffer,
                        path_points_textured_buffer: &intermediary_state
                            .path_points_textured_buffer,
                        text_buffer: &intermediary_state.text_buffer,
                        theme: &draw_state.theme,
                        transform: &curr_ctxt.transform,
                        fill_tessellator: &mut fill_tessellator,
                        stroke_tessellator: &mut stroke_tessellator,
                        glyph_cache: &mut glyph_cache,
                        output_attachment_size: Vec2::new(px_to_pt(w_px), px_to_pt(h_px)),
                        output_attachment_scale_factor: scale_factor,
                    };

                    // Render the primitive.
                    let render = prim.render_primitive(ctxt, &mut mesh);

                    // If the mesh indices are unchanged, there's nothing to be drawn.
                    if prev_index_count == mesh.indices().len() as u32 {
                        assert_eq!(
                            prev_vert_count,
                            mesh.vertex_count(),
                            "vertices were submitted during `render` without submitting indices",
                        );
                        continue;
                    }

                    let new_pipeline_key = {
                        let topology = curr_ctxt.topology;
                        NannouPipelineKey {
                            output_color_format: if view.hdr {
                                ViewTarget::TEXTURE_FORMAT_HDR
                            } else {
                                wgpu::TextureFormat::bevy_default()
                            },
                            sample_count: msaa.samples(),
                            depth_format: depth.texture.format(),
                            topology,
                            blend_state: curr_ctxt.blend,
                        }
                    };

                    let new_pipeline_id =
                        pipelines.specialize(&pipeline_cache, &pipeline, new_pipeline_key);
                    let new_scissor = curr_ctxt.scissor;

                    // Determine which have changed and in turn which require submitting new
                    // commands.
                    let pipeline_changed = Some(new_pipeline_id) != curr_pipeline_id;
                    // let bind_group_changed = Some(new_bind_group_id) != curr_texture_handle;
                    let scissor_changed = Some(new_scissor) != curr_scissor;

                    // If we require submitting a scissor, pipeline or bind group command, first
                    // draw whatever pending vertices we have collected so far. If there have been
                    // no graphics yet, this will do nothing.
                    if scissor_changed || pipeline_changed {
                        push_draw_cmd(
                            &mut curr_start_index,
                            prev_index_count,
                            &mut render_commands,
                        );
                    }

                    // If necessary, push a new pipeline command.
                    if pipeline_changed {
                        curr_pipeline_id = Some(new_pipeline_id);
                        let cmd = RenderCommand::SetPipeline(new_pipeline_id);
                        render_commands.push(cmd);
                    }

                    // // If necessary, push a new bind group command.
                    // if bind_group_changed {
                    //     curr_texture_handle = Some(new_bind_group_id);
                    //     new_tex_sampler_combos.insert(new_bind_group_id, new_pipeline_id);
                    //     let cmd = RenderCommand::SetBindGroup(new_bind_group_id);
                    //     self.render_commands.push(cmd);
                    // }

                    render_commands.push(RenderCommand::SetBindGroup(
                        (**default_texture_handle).clone(),
                    ));

                    // If necessary, push a new scissor command.
                    if scissor_changed {
                        curr_scissor = Some(new_scissor);
                        let rect = match curr_ctxt.scissor {
                            draw::Scissor::Full => full_rect,
                            draw::Scissor::Rect(rect) => full_rect
                                .overlap(rect)
                                .unwrap_or(geom::Rect::from_w_h(0.0, 0.0)),
                            draw::Scissor::NoOverlap => geom::Rect::from_w_h(0.0, 0.0),
                        };
                        let [left, bottom] = window_to_scissor(rect.bottom_left().into());
                        let (width, height) = rect.w_h();
                        let (width, height) = (pt_to_px(width), pt_to_px(height));
                        let scissor = Scissor {
                            left,
                            bottom,
                            width,
                            height,
                        };
                        let cmd = RenderCommand::SetScissor(scissor);
                        render_commands.push(cmd);
                    }

                    // Extend the vertex mode channel.
                    let mode = render.vertex_mode;
                    let new_vs = mesh
                        .points()
                        .len()
                        .saturating_sub(pipeline.vertex_mode_buffer.len());
                    pipeline
                        .vertex_mode_buffer
                        .extend((0..new_vs).map(|_| mode));
                }
            }
        }

        // Insert the final draw command if there is still some drawing to be done.
        push_draw_cmd(
            &mut curr_start_index,
            mesh.indices().len() as u32,
            &mut render_commands,
        );
        commands.entity(entity).insert((mesh, render_commands));
    }
}

// Prepare our uniform bind group from Bevy's view uniforms
fn prepare_view_uniform(
    mut commands: Commands,
    render_device: Res<RenderDevice>,
    pipline: Res<NannouPipeline>,
    view_uniforms: Res<ViewUniforms>,
) {
    if let Some(binding) = view_uniforms.uniforms.binding() {
        commands.insert_resource(ViewUniformBindGroup::new(
            &render_device,
            &pipline.view_bind_group_layout,
            binding,
        ));
    }
}

// Resource wrapper for our view uniform bind group
#[derive(Resource)]
struct ViewUniformBindGroup {
    bind_group: wgpu::BindGroup,
}

impl ViewUniformBindGroup {
    fn new(
        device: &RenderDevice,
        layout: &wgpu::BindGroupLayout,
        binding: wgpu::BindingResource,
    ) -> ViewUniformBindGroup {
        let bind_group = bevy_nannou_wgpu::BindGroupBuilder::new()
            .binding(binding)
            .build(device, layout);

        ViewUniformBindGroup { bind_group }
    }
}

#[derive(Component, Deref, DerefMut, Default, Debug)]
pub struct ViewMesh(draw::Mesh);

/// Commands that map to wgpu encodable commands.
#[derive(Debug, Clone)]
pub enum RenderCommand {
    /// Change pipeline for the new blend mode and topology.
    SetPipeline(CachedRenderPipelineId),
    /// Change bind group for a new image.
    SetBindGroup(Handle<Image>),
    /// Set the rectangular scissor.
    SetScissor(Scissor),
    /// Draw the given vertex range.
    DrawIndexed {
        start_vertex: i32,
        index_range: std::ops::Range<u32>,
    },
}

#[derive(Component, Deref, DerefMut, Default)]
pub struct ViewRenderCommands(Vec<RenderCommand>);
