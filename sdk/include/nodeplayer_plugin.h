/*
 * nodeplayer plugin ABI, version 2 (version 1 plugins still load; 2 added MIDI to nodes).
 * Mirrors sdk/src/abi.rs; see there for details.
 *
 * A plugin is a shared library exporting
 *     const NpPluginDesc *nodeplayer_plugin_entry(void);
 * placed in ~/.local/share/nodeplayer/plugins/ (or a folder in $NODEPLAYER_PLUGIN_PATH).
 *
 * Threads: a node's process() and set_param() run on the audio thread; everything else,
 * including every NpHost and NpUi call, on the UI thread.
 */
#ifndef NODEPLAYER_PLUGIN_H
#define NODEPLAYER_PLUGIN_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define NP_ABI_VERSION 2u

/* UTF-8 text, not NUL-terminated. */
typedef struct NpStr {
    const char *ptr;
    size_t len;
} NpStr;

/* A sink for bytes the callee produces; write may be called repeatedly. */
typedef struct NpBuf {
    void *ctx;
    void (*write)(void *ctx, const uint8_t *data, size_t len);
} NpBuf;

typedef struct NpHost NpHost;
typedef struct NpUi NpUi;

/* ---- Nodes ---------------------------------------------------------------- */

/* A short MIDI message (no sysex) at frame `time` of the block. */
typedef struct NpMidiEvent {
    uint32_t time;
    uint8_t data[3];
    uint8_t len; /* bytes used in data, 1 to 3 */
} NpMidiEvent;

/* Where a node puts its MIDI output; push returns false when the block's buffer is full. */
typedef struct NpMidiOut {
    void *ctx;
    bool (*push)(void *ctx, const NpMidiEvent *event);
} NpMidiOut;

#define NP_MIDI_IN (1u << 0)
#define NP_MIDI_OUT (1u << 1)

#define NP_PARAM_LOG (1u << 0)
#define NP_PARAM_HIDDEN (1u << 1)

typedef struct NpParamInfo {
    uint32_t id;
    const char *name;
    const char *unit;
    double min;
    double max;
    double def;
    uint32_t steps; /* 0 = continuous */
    uint32_t flags;
} NpParamInfo;

typedef struct NpNodeDesc {
    const char *id; /* unique within the plugin; saved graphs refer to it */
    const char *name;
    const char *category; /* "Fx|Distortion" */
    const NpParamInfo *params;
    uint32_t param_count;
    void *(*create)(float sample_rate, uint32_t max_block);
    void (*destroy)(void *inst);
    void (*process)(void *inst, const float *in_l, const float *in_r, float *out_l, float *out_r, uint32_t n);
    void (*set_param)(void *inst, uint32_t id, double value);
    double (*get_param)(void *inst, uint32_t id);
    void (*draw)(void *inst, const NpUi *ui);                       /* optional */
    void (*save_state)(void *inst, NpBuf *out);                     /* optional */
    bool (*load_state)(void *inst, const uint8_t *data, size_t len); /* optional */
    uint32_t (*latency)(void *inst);                                 /* optional */
    /* ABI 2 */
    uint32_t midi; /* NP_MIDI_IN | NP_MIDI_OUT: MIDI pins on the node */
    /* Audio thread, before process() in every block (also without input events). Optional. */
    void (*process_midi)(void *inst, const NpMidiEvent *in, uint32_t count, const NpMidiOut *out);
} NpNodeDesc;

/* ---- Extensions ----------------------------------------------------------- */

#define NP_EVENT_TRACK_CHANGED 1u
#define NP_EVENT_PLAYBACK_STARTED 2u
#define NP_EVENT_PLAYBACK_PAUSED 3u
#define NP_EVENT_TRACK_ENDED 4u
#define NP_EVENT_LIBRARY_CHANGED 5u

typedef struct NpEvent {
    uint32_t kind;
} NpEvent;

typedef struct NpExtension {
    void *(*create)(const NpHost *host);
    void (*destroy)(void *ext);
    void (*idle)(void *ext);                             /* optional */
    void (*on_event)(void *ext, const NpEvent *event);   /* optional */
    const char *panel_title;                             /* NULL = no panel */
    void (*draw_panel)(void *ext, const NpUi *ui);       /* optional */
    void (*on_menu)(void *ext, uint32_t id);             /* optional */
} NpExtension;

typedef struct NpPluginDesc {
    uint32_t abi_version; /* NP_ABI_VERSION */
    const char *name;
    const char *version;
    const char *author;
    const NpNodeDesc *nodes;
    uint32_t node_count;
    const NpExtension *extension; /* NULL = none */
} NpPluginDesc;

/* ---- Host ----------------------------------------------------------------- */

typedef struct NpStatus {
    bool has_track;
    bool playing;
    double position;
    double duration;
    int32_t track_index; /* -1 = not in the library */
    uint32_t library_len;
} NpStatus;

#define NP_TRACK_PATH 0u
#define NP_TRACK_TITLE 1u
#define NP_TRACK_ARTIST 2u
#define NP_TRACK_ALBUM 3u

#define NP_CMD_PLAY 1u
#define NP_CMD_PAUSE 2u
#define NP_CMD_TOGGLE 3u
#define NP_CMD_STOP 4u
#define NP_CMD_NEXT 5u
#define NP_CMD_PREVIOUS 6u
#define NP_CMD_SEEK 7u        /* number: seconds */
#define NP_CMD_LOAD_FILE 8u   /* text: path */
#define NP_CMD_PLAY_TRACK 9u  /* number: library index */
#define NP_CMD_OPEN_FOLDER 10u /* text: path */
#define NP_CMD_LOAD_PRESET 11u /* text: name */
#define NP_CMD_SAVE_PRESET 12u /* text: name */
#define NP_CMD_SET_VOLUME 13u  /* number: dB */

#define NP_LOG_INFO 0u
#define NP_LOG_WARN 1u
#define NP_LOG_ERROR 2u

struct NpHost {
    uint32_t abi_version;
    void *host_data;
    void (*log)(const NpHost *host, uint32_t level, NpStr msg);
    void (*status)(const NpHost *host, NpStatus *out);
    void (*track_text)(const NpHost *host, uint32_t field, NpBuf *out);
    bool (*library_path)(const NpHost *host, uint32_t index, NpBuf *out);
    bool (*library_root)(const NpHost *host, NpBuf *out);
    bool (*command)(const NpHost *host, uint32_t cmd, double number, NpStr text);
    uint32_t (*add_menu_item)(const NpHost *host, NpStr label);
    void (*request_repaint)(const NpHost *host);
    void (*data_dir)(const NpHost *host, NpBuf *out);
};

/* ---- UI ------------------------------------------------------------------- */

typedef void (*NpUiFn)(const NpUi *ui, void *user);

#define NP_TEXT_NORMAL 0u
#define NP_TEXT_HEADING 1u
#define NP_TEXT_SMALL 2u
#define NP_TEXT_WEAK 3u
#define NP_TEXT_MONO 4u

#define NP_SLIDER_LOG (1u << 0)
#define NP_SLIDER_INTEGER (1u << 1)

#define NP_LAYOUT_HORIZONTAL 0u
#define NP_LAYOUT_VERTICAL 1u
#define NP_LAYOUT_GROUP 2u
#define NP_LAYOUT_COLLAPSING 3u /* title */
#define NP_LAYOUT_SCROLL 4u     /* number: max height */
#define NP_LAYOUT_ID_SCOPE 5u   /* number: id */

typedef struct NpCanvas {
    float x, y, w, h;
    bool hovered, clicked, double_clicked, dragged;
    float drag_dx, drag_dy;
    float pointer_x, pointer_y;
    float scroll_y;
} NpCanvas;

#define NP_PAINT_LINE 0u
#define NP_PAINT_RECT 1u
#define NP_PAINT_RECT_STROKE 2u
#define NP_PAINT_CIRCLE 3u
#define NP_PAINT_CIRCLE_STROKE 4u
#define NP_PAINT_TEXT 5u

#define NP_ALIGN_LEFT 0u
#define NP_ALIGN_CENTER 1u
#define NP_ALIGN_RIGHT 2u

typedef struct NpPaint {
    uint32_t kind;
    float x1, y1, x2, y2;
    float size;    /* line width, radius or text size */
    uint32_t rgba; /* 0xRRGGBBAA */
    uint32_t align;
    NpStr text;
} NpPaint;

typedef struct NpUiVtbl {
    void (*label)(const NpUi *ui, NpStr text, uint32_t style);
    void (*colored_label)(const NpUi *ui, NpStr text, uint32_t rgba);
    bool (*button)(const NpUi *ui, NpStr text, bool small);
    bool (*selectable)(const NpUi *ui, NpStr text, bool selected);
    bool (*checkbox)(const NpUi *ui, NpStr text, bool *value);
    bool (*slider)(const NpUi *ui, NpStr text, double *value, double min, double max, uint32_t flags);
    bool (*drag_value)(const NpUi *ui, NpStr prefix, double *value, double speed, double min, double max, NpStr suffix);
    bool (*text_edit)(const NpUi *ui, uint8_t *buf, size_t cap, size_t *len, NpStr hint);
    bool (*combo)(const NpUi *ui, NpStr id, uint32_t *selected, NpStr items /* '\n'-separated */);
    void (*separator)(const NpUi *ui);
    void (*space)(const NpUi *ui, float amount);
    void (*progress)(const NpUi *ui, float fraction, NpStr text);
    void (*layout)(const NpUi *ui, uint32_t kind, NpStr title, double number, NpUiFn body, void *user);
    float (*available_width)(const NpUi *ui);
    void (*canvas)(const NpUi *ui, float w, float h, NpCanvas *out);
    void (*paint)(const NpUi *ui, const NpPaint *cmd);
    void (*request_repaint)(const NpUi *ui);
} NpUiVtbl;

struct NpUi {
    void *ctx;
    const NpUiVtbl *vt;
};

/* Convenience for string literals. */
#define NP_STR(s) ((NpStr){(s), sizeof(s) - 1})

#ifdef __cplusplus
}
#endif

#endif
