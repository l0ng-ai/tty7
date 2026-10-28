// The single header bindgen chews on: everything libghostty-vt publishes,
// through the same umbrella include a C embedder uses. regen-bindings.sh
// passes this file with -I$TTY7_GHOSTTY_DIR/include, so the generated
// bindings always describe the headers of the exact pinned build.
#include <ghostty/vt.h>
