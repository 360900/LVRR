#version 310 es
precision highp float;

layout (location = 0) in vec2 in_uv;
layout (location = 0) out vec4 out_color;

layout (set = 0, binding = 0) uniform sampler2D in_texture;

vec3 srgb_to_linear(vec3 color)
{
    bvec3 cutoff = lessThan(color, vec3(0.04045));
    vec3 higher = pow((color + vec3(0.055)) / vec3(1.055), vec3(2.4));
    vec3 lower = color / vec3(12.92);
    return mix(higher, lower, cutoff);
}

void main()
{
    vec4 color = texture(in_texture, in_uv);

    // wl_surface buffer contents use premultiplied alpha
    if (color.a > 0.0) {
        color.rgb = srgb_to_linear(clamp(color.rgb / color.a, 0.0, 1.0)) * color.a;
    } else {
        color.rgb = vec3(0.0);
    }

    out_color = color;
}
