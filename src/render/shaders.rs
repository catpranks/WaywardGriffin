//! GLSL sources compiled at build time, plus their push-constant layouts.

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(super) struct TexturedPushConstants {
    pub(super) opaque: u32,
}

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(super) struct BorderPushConstants {
    pub(super) time: f32,
    pub(super) content_width: f32,
    pub(super) content_height: f32,
}

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub(super) struct ScreenshotPushConstants {
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) mod vs {
    vulkano_shaders::shader! {
        ty: "vertex",
        src: r"
            #version 450

            layout(location = 0) out vec2 tex_coords;

            void main() {
                float x = float((gl_VertexIndex & 1) << 2) - 1.0;
                float y = float((gl_VertexIndex & 2) << 1) - 1.0;

                gl_Position = vec4(x, y, 0.0, 1.0);
                tex_coords = gl_Position.xy * 0.5 + 0.5;
            }
        ",
    }
}

pub(super) mod fs_textured {
    vulkano_shaders::shader! {
        ty: "fragment",
        src: r"
            #version 450

            layout(location = 0) in vec2 tex_coords;
            layout(location = 0) out vec4 f_color;

            layout(set = 0, binding = 0) uniform sampler s;
            layout(set = 0, binding = 1) uniform texture2D tex;

            layout(push_constant) uniform PushConstants {
                uint opaque;
            } pc;

            void main() {
                f_color = texture(sampler2D(tex, s), tex_coords);
                if (pc.opaque != 0u) {
                    f_color.a = 1.0;
                }
            }
        ",
    }
}

pub(super) mod cs_screenshot {
    vulkano_shaders::shader! {
        ty: "compute",
        src: r"
            #version 450

            layout(local_size_x = 16, local_size_y = 16) in;

            layout(set = 0, binding = 0) uniform sampler s;
            layout(set = 0, binding = 1) uniform texture2D tex;

            layout(set = 0, binding = 2) buffer OutputBuf {
                uint data[];
            } out_buf;

            layout(push_constant) uniform PushConstants {
                uint width;
                uint height;
            } pc;

            vec3 linear_to_srgb(vec3 c) {
                vec3 lo = c * 12.92;
                vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
                return mix(lo, hi, greaterThan(c, vec3(0.0031308)));
            }

            void main() {
                uint x = gl_GlobalInvocationID.x;
                uint y = gl_GlobalInvocationID.y;
                if (x >= pc.width || y >= pc.height) return;

                vec2 uv = (vec2(x, y) + 0.5) / vec2(pc.width, pc.height);
                vec4 color = texture(sampler2D(tex, s), uv);
                color.rgb = linear_to_srgb(color.rgb);
                uvec4 c = clamp(uvec4(color * 255.0 + 0.5), uvec4(0), uvec4(255));
                out_buf.data[y * pc.width + x] = c.b | (c.g << 8u) | (c.r << 16u) | (255u << 24u);
            }
        ",
    }
}

pub(super) mod fs_border {
    vulkano_shaders::shader! {
        ty: "fragment",
        src: r"
            #version 450

            layout(location = 0) in vec2 tex_coords;
            layout(location = 0) out vec4 f_color;

            layout(push_constant) uniform PushConstants {
                float time;
                float content_width;
                float content_height;
            } pc;

            void main() {
                float sine_val = (sin(pc.time * 2.0) + 1.0) / 2.0;
                float border_width_px = 15.0 + sine_val * 10.0;
                float alpha = (5.0 + sine_val * 150.0) / 255.0;

                float px = tex_coords.x * pc.content_width;
                float py = tex_coords.y * pc.content_height;

                float edge_dist = min(min(px, pc.content_width - px),
                                      min(py, pc.content_height - py));

                if (edge_dist < border_width_px) {
                    f_color = vec4(0.0, 0.78, 0.78, alpha);
                } else {
                    f_color = vec4(0.0);
                }
            }
        ",
    }
}
