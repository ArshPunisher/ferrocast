// Spike (Track B phase 0): drive OBS's engine (libobs from OBS.app) with no OBS window.
// 1. start libobs and load every plugin  2. list every source type with its options
// 3. build a scene (color + text) and record 5 s to an MP4 with x264 + AAC.
#include <obs.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define APP "/Applications/OBS.app/Contents"

static int warnings = 0;
static void log_handler(int lvl, const char *fmt, va_list args, void *p)
{
	(void)p;
	if (lvl > LOG_WARNING)
		return;
	warnings++;
	fprintf(stderr, "[obs %s] ", lvl == LOG_ERROR ? "error" : "warn");
	vfprintf(stderr, fmt, args);
	fputc('\n', stderr);
}

static const char *ptype(enum obs_property_type t)
{
	switch (t) {
	case OBS_PROPERTY_BOOL: return "bool";
	case OBS_PROPERTY_INT: return "int";
	case OBS_PROPERTY_FLOAT: return "float";
	case OBS_PROPERTY_TEXT: return "text";
	case OBS_PROPERTY_PATH: return "path";
	case OBS_PROPERTY_LIST: return "list";
	case OBS_PROPERTY_COLOR: return "color";
	case OBS_PROPERTY_BUTTON: return "button";
	case OBS_PROPERTY_FONT: return "font";
	case OBS_PROPERTY_EDITABLE_LIST: return "editable_list";
	case OBS_PROPERTY_FRAME_RATE: return "frame_rate";
	case OBS_PROPERTY_GROUP: return "group";
	case OBS_PROPERTY_COLOR_ALPHA: return "color_alpha";
	default: return "other";
	}
}

static void list_props(FILE *f, obs_properties_t *props, const char *indent)
{
	for (obs_property_t *p = obs_properties_first(props); p; obs_property_next(&p)) {
		fprintf(f, "%s- %s (%s): %s", indent, obs_property_name(p), ptype(obs_property_get_type(p)),
			obs_property_description(p) ? obs_property_description(p) : "");
		if (obs_property_get_type(p) == OBS_PROPERTY_LIST)
			fprintf(f, " [%zu choices]", obs_property_list_item_count(p));
		fputc('\n', f);
		if (obs_property_get_type(p) == OBS_PROPERTY_GROUP)
			list_props(f, obs_property_group_content(p), "    ");
	}
}

int main(int argc, char **argv)
{
	const char *out_dir = argc > 1 ? argv[1] : ".";
	char path[1024];
	setvbuf(stdout, NULL, _IONBF, 0);
	base_set_log_handler(log_handler, NULL);

	snprintf(path, sizeof(path), "%s/obs-config", out_dir);
	char modcfg[1024];
	snprintf(modcfg, sizeof(modcfg), "%s/plugin-config", out_dir);
	if (!obs_startup("en-US", path, NULL) && !obs_initialized()) {
		fprintf(stderr, "obs_startup failed\n");
		return 1;
	}
	obs_add_data_path(APP "/Frameworks/libobs.framework/Resources/");
	printf("libobs %s started\n", obs_get_version_string());

	struct obs_audio_info ai = {.samples_per_sec = 48000, .speakers = SPEAKERS_STEREO};
	if (!obs_reset_audio(&ai)) {
		fprintf(stderr, "audio reset failed\n");
		return 1;
	}
	struct obs_video_info vi = {
		.graphics_module = APP "/Frameworks/libobs-opengl.dylib",
		.fps_num = 30, .fps_den = 1,
		.base_width = 1280, .base_height = 720,
		.output_width = 1280, .output_height = 720,
		.output_format = VIDEO_FORMAT_NV12,
		.adapter = 0, .gpu_conversion = true,
		.colorspace = VIDEO_CS_709, .range = VIDEO_RANGE_PARTIAL,
		.scale_type = OBS_SCALE_BICUBIC,
	};
	int vr = obs_reset_video(&vi);
	if (vr != OBS_VIDEO_SUCCESS) {
		fprintf(stderr, "video reset failed: %d\n", vr);
		return 1;
	}
	printf("video: 1280x720 30 fps on %s\n", obs_get_video() ? "OpenGL" : "?");

	obs_add_module_path(APP "/PlugIns/%module%.plugin/Contents/MacOS/",
			    APP "/PlugIns/%module%.plugin/Contents/Resources/");
	// Interface-only plugins need OBS's Qt window; the Studio has its own interface.
	const char *ui_only[] = {"frontend-tools", "decklink-output-ui", "aja-output-ui", "decklink-captions",
				 "obs-websocket", "aja", "decklink", "obs-browser", NULL};
	for (int i = 0; ui_only[i]; i++)
		obs_add_disabled_module(ui_only[i]);
	obs_load_all_modules();
	obs_post_load_modules();

	// 2. every input, filter and transition with its options
	snprintf(path, sizeof(path), "%s/source-types.md", out_dir);
	FILE *f = fopen(path, "w");
	const char *id;
	int inputs = 0, filters = 0, transitions = 0, options = 0;
	for (size_t i = 0; obs_enum_input_types(i, &id); i++, inputs++) {
		// Capture plugins read their device list from a live source (as OBS's own dialog does);
		// asked without one they crash, and making one would ask for camera/screen permission.
		bool needs_instance = strstr(id, "avcapture") || strstr(id, "av_capture") ||
				      !strcmp(id, "display_capture") || !strcmp(id, "screen_capture") ||
				      !strcmp(id, "window_capture") || !strcmp(id, "sck_audio_capture") ||
				      strstr(id, "coreaudio") || strstr(id, "syphon");
		obs_properties_t *props = needs_instance ? NULL : obs_get_source_properties(id);
		fprintf(f, "## input %s: %s\n", id, obs_source_get_display_name(id)); fflush(f);
		if (props) {
			list_props(f, props, "  ");
			for (obs_property_t *p = obs_properties_first(props); p; obs_property_next(&p))
				options++;
			obs_properties_destroy(props);
		}
	}
	for (size_t i = 0; obs_enum_filter_types(i, &id); i++, filters++) {
		// Options come from a live (private) instance, the way OBS's own dialog asks.
		obs_source_t *tmp = obs_source_create_private(id, "probe", NULL);
		obs_properties_t *props = tmp ? obs_source_properties(tmp) : NULL;
		fprintf(f, "## filter %s: %s\n", id, obs_source_get_display_name(id));
		if (props) {
			list_props(f, props, "  ");
			for (obs_property_t *p = obs_properties_first(props); p; obs_property_next(&p))
				options++;
			obs_properties_destroy(props);
		}
		obs_source_release(tmp);
	}
	for (size_t i = 0; obs_enum_transition_types(i, &id); i++, transitions++)
		fprintf(f, "## transition %s: %s\n", id, obs_source_get_display_name(id));
	int encoders = 0, outputs = 0;
	for (size_t i = 0; obs_enum_encoder_types(i, &id); i++, encoders++)
		fprintf(f, "## encoder %s: %s\n", id, obs_encoder_get_display_name(id));
	for (size_t i = 0; obs_enum_output_types(i, &id); i++, outputs++)
		fprintf(f, "## output %s\n", id);
	fclose(f);
	printf("types: %d inputs, %d filters, %d transitions, %d encoders, %d outputs; %d top-level options\n",
	       inputs, filters, transitions, encoders, outputs, options);

	// 3. a scene: a colour background and a text, recorded for 5 s
	obs_scene_t *scene = obs_scene_create("Spike");
	obs_data_t *s = obs_data_create();
	obs_data_set_int(s, "color", 0xFFA33FFF); // ABGR: Ferrocast pink
	obs_data_set_int(s, "width", 1280);
	obs_data_set_int(s, "height", 720);
	obs_source_t *bg = obs_source_create("color_source", "Background", s, NULL);
	obs_data_release(s);
	obs_scene_add(scene, bg);

	s = obs_data_create();
	obs_data_set_string(s, "text", "Ferrocast Studio spike");
	obs_data_t *font = obs_data_create();
	obs_data_set_string(font, "face", "Helvetica");
	obs_data_set_int(font, "size", 96);
	obs_data_set_obj(s, "font", font);
	obs_data_release(font);
	obs_source_t *txt = obs_source_create("text_ft2_source", "Title", s, NULL);
	obs_data_release(s);
	obs_sceneitem_t *item = obs_scene_add(scene, txt);
	struct vec2 pos = {.x = 120, .y = 300};
	obs_sceneitem_set_pos(item, &pos);
	obs_set_output_source(0, obs_scene_get_source(scene));

	obs_encoder_t *venc = obs_video_encoder_create("obs_x264", "video", NULL, NULL);
	obs_encoder_t *aenc = obs_audio_encoder_create("ffmpeg_aac", "audio", NULL, 0, NULL);
	if (!aenc)
		aenc = obs_audio_encoder_create("CoreAudio_AAC", "audio", NULL, 0, NULL);
	obs_encoder_set_video(venc, obs_get_video());
	obs_encoder_set_audio(aenc, obs_get_audio());

	s = obs_data_create();
	snprintf(path, sizeof(path), "%s/spike.mp4", out_dir);
	obs_data_set_string(s, "path", path);
	obs_output_t *rec = obs_output_create("mp4_output", "record", s, NULL);
	obs_data_release(s);
	obs_output_set_video_encoder(rec, venc);
	obs_output_set_audio_encoder(rec, aenc, 0);
	if (!obs_output_start(rec)) {
		fprintf(stderr, "recording didn't start: %s\n", obs_output_get_last_error(rec));
		return 1;
	}
	printf("recording %s ...\n", path);
	sleep(5);
	obs_output_stop(rec);
	for (int i = 0; i < 50 && obs_output_active(rec); i++)
		usleep(100000);
	printf("recorded, frames rendered %u lagged %u\n", obs_get_total_frames(), obs_get_lagged_frames());

	obs_output_release(rec);
	obs_encoder_release(venc);
	obs_encoder_release(aenc);
	obs_set_output_source(0, NULL);
	obs_source_release(bg);
	obs_source_release(txt);
	obs_scene_release(scene);
	obs_shutdown();
	printf("done, %d warnings\n", warnings);
	return 0;
}
