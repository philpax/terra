
uniform mat4 invModelViewProjection;

out vec3 ray;

void main() {
	if(gl_VertexID == 0) gl_Position = vec4(-1, -1, 1, 1);
	if(gl_VertexID == 1) gl_Position = vec4(-1,  1, 1, 1);
	if(gl_VertexID == 2) gl_Position = vec4( 1,  1, 1, 1);

	if(gl_VertexID == 3) gl_Position = vec4(-1, -1, 1, 1);
	if(gl_VertexID == 5) gl_Position = vec4( 1,  1, 1, 1);
	if(gl_VertexID == 4) gl_Position = vec4( 1, -1, 1, 1);

	vec4 hr0 = invModelViewProjection * vec4(gl_Position.xy, 0, 1);
	vec4 hr1 = invModelViewProjection * vec4(gl_Position.xy, 1, 1);
	ray = hr1.xyz / hr1.w - hr0.xyz / hr0.w;
}
