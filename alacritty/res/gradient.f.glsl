#define GLES2_RENDERER
varying mediump vec2 vUv;

uniform mediump vec3 gradientStart;
uniform mediump vec3 gradientEnd;
uniform int gradientDirection;
uniform mediump float opacity;
uniform mediump float borderRadius;
uniform mediump vec2 windowSize;

mediump float roundedBoxSdf(mediump vec2 p, mediump vec2 center, mediump vec2 halfSize, mediump float radius) {
    mediump vec2 d = abs(p - center) - halfSize + radius;
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - radius;
}

void main() {
    mediump float t;
    if (gradientDirection == 0) {
        t = 1.0 - vUv.y;
    } else if (gradientDirection == 1) {
        t = vUv.x;
    } else {
        t = (vUv.x + 1.0 - vUv.y) * 0.5;
    }

    mediump vec3 color = mix(gradientStart, gradientEnd, clamp(t, 0.0, 1.0));

    mediump float alpha = opacity;
    if (borderRadius > 0.0) {
        mediump vec2 pixelPos = vUv * windowSize;
        mediump float distance = roundedBoxSdf(pixelPos, windowSize * 0.5, windowSize * 0.5, borderRadius);
        alpha *= 1.0 - smoothstep(-1.0, 1.0, distance);
    }

    gl_FragColor = vec4(color * alpha, alpha);
}
