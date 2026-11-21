/* Network-free C ABI fixture; not an XMPP parser or production backend. */
#define _POSIX_C_SOURCE 200809L
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

typedef struct Conn Conn;
typedef struct { Conn *conn; } Ctx;
typedef struct { char *xml; char name[32], id[160], to[256], from[256], type[32]; } Stanza;
typedef void (*ConnHandler)(void *, int, int, void *, void *);
typedef int (*StanzaHandler)(void *, void *, void *);
struct Conn { Ctx *ctx; long flags; char *jid; ConnHandler callback; void *userdata; StanzaHandler stanza; void *stanza_data; char *pending; };

static void attribute(const char *xml, const char *name, char *out, size_t cap) {
    char key[64]; snprintf(key, sizeof(key), " %s='", name);
    const char *start = strstr(xml, key);
    if (!start) { out[0] = 0; return; }
    start += strlen(key); const char *end = strchr(start, '\'');
    size_t len = end ? (size_t)(end - start) : 0;
    if (len >= cap) len = cap - 1;
    memcpy(out, start, len); out[len] = 0;
}
void xmpp_initialize(void) {}
int xmpp_version_check(int major, int minor) { return major == 0 && minor <= 12; }
void *xmpp_ctx_new(const void *mem, const void *log) { (void)mem; (void)log; return calloc(1, sizeof(Ctx)); }
void xmpp_ctx_free(void *ctx) { free(ctx); }
void xmpp_free(void *ctx, void *p) { (void)ctx; free(p); }
void *xmpp_conn_new(void *ctx) { Conn *c = calloc(1, sizeof(*c)); c->ctx = ctx; ((Ctx *)ctx)->conn = c; return c; }
int xmpp_conn_release(void *pointer) { Conn *c = pointer; free(c->jid); free(c->pending); free(c); return 1; }
void xmpp_conn_set_jid(void *pointer, const char *jid) { ((Conn *)pointer)->jid = strdup(jid); }
void xmpp_conn_set_pass(void *pointer, const char *pass) { (void)pointer; (void)pass; }
void xmpp_conn_set_cafile(void *pointer, const char *path) { (void)pointer; (void)path; }
int xmpp_conn_set_flags(void *pointer, long flags) { ((Conn *)pointer)->flags = flags; return flags == 2 ? 0 : -1; }
int xmpp_conn_is_secured(void *pointer) { return ((Conn *)pointer)->flags == 2; }
const char *xmpp_conn_get_bound_jid(void *pointer) { return ((Conn *)pointer)->jid; }
int xmpp_connect_client(void *pointer, const char *host, unsigned short port, ConnHandler callback, void *userdata) {
    (void)host; (void)port; Conn *c = pointer; c->callback = callback; c->userdata = userdata;
    callback(c, 0, 0, NULL, userdata); return 0;
}
void xmpp_disconnect(void *pointer) { Conn *c = pointer; if (c->callback) c->callback(c, 2, 0, NULL, c->userdata); }
void xmpp_handler_add(void *pointer, StanzaHandler callback, const char *ns, const char *name, const char *type, void *userdata) {
    (void)ns; (void)name; (void)type; Conn *c = pointer; c->stanza = callback; c->stanza_data = userdata;
}
void *xmpp_stanza_new_from_string(void *ctx, const char *xml) {
    (void)ctx; if (!xml || xml[0] != '<') return NULL;
    Stanza *s = calloc(1, sizeof(*s)); s->xml = strdup(xml);
    size_t n = strcspn(xml + 1, " />\t\r\n"); if (n >= sizeof(s->name)) n = sizeof(s->name) - 1;
    memcpy(s->name, xml + 1, n);
    attribute(xml, "id", s->id, sizeof(s->id)); attribute(xml, "to", s->to, sizeof(s->to));
    attribute(xml, "from", s->from, sizeof(s->from)); attribute(xml, "type", s->type, sizeof(s->type));
    return s;
}
int xmpp_stanza_release(void *pointer) { Stanza *s = pointer; free(s->xml); free(s); return 1; }
int xmpp_stanza_to_text(void *pointer, char **buffer, size_t *length) { Stanza *s = pointer; *buffer = strdup(s->xml); *length = strlen(s->xml); return 0; }
const char *xmpp_stanza_get_name(void *pointer) { return ((Stanza *)pointer)->name; }
const char *xmpp_stanza_get_attribute(void *pointer, const char *name) {
    Stanza *s = pointer;
    if (!strcmp(name, "id")) return s->id;
    if (!strcmp(name, "to")) return s->to;
    if (!strcmp(name, "from")) return s->from;
    if (!strcmp(name, "type")) return s->type;
    return NULL;
}
int xmpp_stanza_set_id(void *pointer, const char *id) { snprintf(((Stanza *)pointer)->id, 160, "%s", id); return 0; }
void *xmpp_stanza_get_child_by_ns(void *pointer, const char *ns) {
    Stanza *s = pointer; static Stanza marker = { .name = "markable" };
    if (!strcmp(ns, "urn:xmpp:chat-markers:0") && strstr(s->xml, "<markable")) return &marker;
    return NULL;
}
void *xmpp_stanza_get_children(void *pointer) { (void)pointer; return NULL; }
void *xmpp_stanza_get_next(void *pointer) { (void)pointer; return NULL; }
char *xmpp_message_get_body(void *pointer) {
    const char *start = strstr(((Stanza *)pointer)->xml, "<body>"); if (!start) return NULL;
    start += 6; const char *end = strstr(start, "</body>"); if (!end) return NULL;
    size_t n = (size_t)(end - start); char *result = malloc(n + 1); memcpy(result, start, n); result[n] = 0; return result;
}
char *xmpp_uuid_gen(void *ctx) { (void)ctx; static unsigned id; char buf[80]; snprintf(buf, sizeof(buf), "fixture-%u", ++id); return strdup(buf); }
void xmpp_send(void *pointer, void *stanza) {
    Conn *c = pointer; Stanza *s = stanza;
    if (!strcmp(s->name, "iq") && (!strcmp(s->type, "get") || !strcmp(s->type, "set"))) {
        char response[512]; snprintf(response, sizeof(response), "<iq type='result' id='%s' from='example.org'/>", s->id);
        free(c->pending); c->pending = strdup(response);
    } else if (!strcmp(s->name, "message") && strstr(s->xml, "<body>")) {
        free(c->pending); c->pending = strdup("<message type='chat' id='remote-1' from='b@example.org/phone'><body>inbound fixture</body><markable xmlns='urn:xmpp:chat-markers:0'/></message>");
    }
}
void xmpp_run_once(void *ctx, unsigned long timeout) {
    (void)timeout; Conn *c = ((Ctx *)ctx)->conn;
    if (c->pending && c->stanza) {
        char *xml = c->pending; c->pending = NULL;
        void *stanza = xmpp_stanza_new_from_string(ctx, xml); free(xml);
        c->stanza(c, stanza, c->stanza_data); xmpp_stanza_release(stanza);
    }
}
