#version 450
layout(set = 0, binding = 0) uniform sampler2D overlay;
layout(push_constant, std430) uniform pc {
    layout(offset = 16) uint decodeSrgb;
};
layout(location = 0) in vec2 texCoord;
layout(location = 0) out vec4 color;

// The output must be linear: runtimes decode sRGB swapchains and treat UNORM
// ones as linear. Sampling an _SRGB texture already decodes it, but UNORM
// textures usually hold sRGB-encoded bytes (typical for game backbuffers), so
// the backend asks for a manual decode for those.
vec3 srgb_to_linear(vec3 c) {
    bvec3 cutoff = lessThan(c, vec3(0.04045));
    vec3 lo = c / vec3(12.92);
    vec3 hi = pow((c + vec3(0.055)) / vec3(1.055), vec3(2.4));
    return mix(hi, lo, cutoff);
}

void main() {
    vec4 sampled = texture(overlay, texCoord);
    color = decodeSrgb != 0u ? vec4(srgb_to_linear(sampled.rgb), sampled.a) : sampled;
}
