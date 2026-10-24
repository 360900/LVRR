#version 450
layout(set = 0, binding = 0) uniform sampler2DMS overlay;
layout(push_constant, std430) uniform pc {
    layout(offset = 16) uint decodeSrgb;
};
layout(location = 0) in vec2 texCoord;
layout(location = 0) out vec4 color;

// See overlay.frag.
vec3 srgb_to_linear(vec3 c) {
    bvec3 cutoff = lessThan(c, vec3(0.04045));
    vec3 lo = c / vec3(12.92);
    vec3 hi = pow((c + vec3(0.055)) / vec3(1.055), vec3(2.4));
    return mix(hi, lo, cutoff);
}

// Multisampled textures can't be filtered, so resolve the nearest texel by averaging its samples.
void main() {
    ivec2 size = textureSize(overlay);
    ivec2 texel = clamp(ivec2(texCoord * vec2(size)), ivec2(0), size - 1);
    int samples = textureSamples(overlay);
    vec4 sampled = vec4(0.0);
    for (int i = 0; i < samples; i++) {
        sampled += texelFetch(overlay, texel, i);
    }
    sampled /= float(samples);
    color = decodeSrgb != 0u ? vec4(srgb_to_linear(sampled.rgb), sampled.a) : sampled;
}
