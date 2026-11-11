//! HDR shaders keep absolute BT.2020/PQ code values; Wayland describes them.

pub(super) const VERTEX: &str = "#version 300 es
in vec2 a_pos;
uniform vec4 u_rect;
out vec2 v_tex;
void main() {
    v_tex = vec2(a_pos.x, 1.0 - a_pos.y);
    gl_Position = vec4(mix(u_rect.xy, u_rect.zw, a_pos), 0.0, 1.0);
}";

const MATRIX: &str = "
vec3 bt2020(float yy, float cb, float cr) {
    float y = (yy - 64.0) / 876.0;
    float u = (cb - 512.0) / 896.0;
    float v = (cr - 512.0) / 896.0;
    return clamp(vec3(y + 1.4746*v, y - 0.1645531268*u - 0.5713531268*v, y + 1.8814*u), 0.0, 1.0);
}";

/// P010 uploaded as GLES3 integer textures (no `EXT_texture_norm16` dependency).
pub(super) fn cpu_fragment() -> String {
    format!(
        "#version 300 es
precision highp float;
precision highp usampler2D;
in vec2 v_tex;
uniform usampler2D u_tex0;
uniform usampler2D u_tex1;
out vec4 out_color;
{MATRIX}
// Integer textures have no hardware linear filtering. Match the normalized
// DMA-BUF path's bilinear Y/UV sampling without reducing P010 precision.
vec2 filtered(usampler2D image, vec2 uv) {{
    ivec2 size = textureSize(image, 0);
    vec2 point = uv * vec2(size) - 0.5;
    ivec2 at = ivec2(floor(point));
    vec2 weight = fract(point);
    vec2 a = vec2(texelFetch(image, clamp(at, ivec2(0), size-1), 0).rg >> 6u);
    vec2 b = vec2(texelFetch(image, clamp(at+ivec2(1,0), ivec2(0), size-1), 0).rg >> 6u);
    vec2 c = vec2(texelFetch(image, clamp(at+ivec2(0,1), ivec2(0), size-1), 0).rg >> 6u);
    vec2 d = vec2(texelFetch(image, clamp(at+ivec2(1,1), ivec2(0), size-1), 0).rg >> 6u);
    return mix(mix(a,b,weight.x), mix(c,d,weight.x), weight.y);
}}
void main() {{
    float y = filtered(u_tex0, v_tex).r;
    vec2 uv = filtered(u_tex1, v_tex);
    out_color = vec4(bt2020(y, uv.r, uv.g), 1.0);
}}"
    )
}

/// EGL's R16/GR1616 DMA-BUF planes are normalized 16-bit textures.
/// Samplers MUST be highp: Mesa's lowp default corrupted P010 into magenta.
pub(super) fn dmabuf_fragment(external: bool) -> String {
    let (extension, sampler) = if external {
        (
            "#extension GL_OES_EGL_image_external : require",
            "samplerExternalOES",
        )
    } else {
        ("", "sampler2D")
    };
    format!(
        "{extension}
precision highp float;
varying vec2 v_tex;
uniform highp {sampler} u_tex0;
uniform highp {sampler} u_tex1;
{MATRIX}
void main() {{
    float y = texture2D(u_tex0, v_tex).r * (65535.0/64.0);
    vec2 uv = texture2D(u_tex1, v_tex).rg * (65535.0/64.0);
    gl_FragColor = vec4(bt2020(y, uv.r, uv.g), 1.0);
}}"
    )
}

/// Opaque SDR UI at 160-nit white, converted to BT.2020/PQ. Discard outside
/// the panel instead of blending in nonlinear PQ space. Desktop calibration
/// and the video's absolute luminance are never changed.
pub(super) const OVERLAY: &str = "precision highp float;
varying vec2 v_tex;
uniform sampler2D u_tex0;
vec3 pq(vec3 nits) {
    vec3 p = pow(max(nits, vec3(0.0)) / 10000.0, vec3(2610.0/16384.0));
    return pow((vec3(3424.0/4096.0) + (2413.0/128.0)*p) /
        (vec3(1.0) + (2392.0/128.0)*p), vec3(2523.0/32.0));
}
void main() {
    vec4 ui = texture2D(u_tex0, v_tex);
    if (ui.a < 0.5) discard;
    vec3 linear = mix(ui.rgb/12.92, pow((ui.rgb+0.055)/1.055, vec3(2.4)), step(vec3(0.04045), ui.rgb));
    mat3 to2020 = mat3(0.627404,0.069097,0.016391, 0.329283,0.919540,0.088013, 0.043313,0.011362,0.895595);
    gl_FragColor = vec4(pq(to2020 * linear * 160.0), 1.0);
}";
