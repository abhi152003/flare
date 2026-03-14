// Fragment shader for gradient background rendering.
in vec2 vUv;

out vec4 FragColor;

// Gradient colors (premultiplied by alpha in the clear pass).
uniform vec3 gradientStart;
uniform vec3 gradientEnd;
// 0 = vertical, 1 = horizontal, 2 = diagonal
uniform int gradientDirection;
// Window opacity for premultiplied alpha.
uniform float opacity;
// Rounded corner radius in normalized coordinates [0, 1].
uniform float borderRadius;
// Window dimensions in pixels.
uniform vec2 windowSize;

// Signed distance field for a rounded rectangle.
float roundedBoxSdf(vec2 p, vec2 center, vec2 halfSize, float radius) {
    vec2 d = abs(p - center) - halfSize + radius;
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - radius;
}

void main() {
    // Compute gradient interpolation factor.
    float t;
    if (gradientDirection == 0) {
        // Vertical: top to bottom.
        t = 1.0 - vUv.y;
    } else if (gradientDirection == 1) {
        // Horizontal: left to right.
        t = vUv.x;
    } else {
        // Diagonal: top-left to bottom-right.
        t = (vUv.x + 1.0 - vUv.y) * 0.5;
    }

    vec3 color = mix(gradientStart, gradientEnd, clamp(t, 0.0, 1.0));

    // Apply rounded corners if radius > 0.
    float alpha = opacity;
    if (borderRadius > 0.0) {
        vec2 pixelPos = vUv * windowSize;
        float distance = roundedBoxSdf(pixelPos, windowSize * 0.5, windowSize * 0.5, borderRadius);
        // Smooth edge with 1px anti-aliasing.
        alpha *= 1.0 - smoothstep(-1.0, 1.0, distance);
    }

    // Premultiplied alpha output.
    FragColor = vec4(color * alpha, alpha);
}
