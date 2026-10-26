/*
 * LVRR: translucent hands with a bright rim, like the runtime's own
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 */

#version 450

#extension GL_GOOGLE_include_directive : require

#include "common.glsl.inc"

// Fragment output
layout(location = 0) out vec4 out_color;

void main()
{
    vec3 n = normalize(normal);
    vec3 view_dir = normalize(-frag_pos.xyz);

    // faint fill facing the eye, bright edge where the surface turns away
    float facing = max(dot(n, view_dir), 0);
    float rim = pow(1 - facing, 2.5);

    vec3 fill = material.base_color_factor.rgb;
    vec3 edge = material.base_emissive_factor.rgb;
    float alpha = material.base_color_factor.a * mix(0.18, 0.95, rim);

    // premultiplied, the eye buffer is blended over passthrough and the panels
    out_color = vec4(mix(fill, edge, rim) * alpha, alpha);
}
