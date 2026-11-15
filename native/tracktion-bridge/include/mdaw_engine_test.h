/* mdaw_engine_test.h — functions for the bridge's own tests (test/bridge_test.c). NOT part of
 * the stable interface of mdaw_engine.h: they may change or go with any version, and an
 * application does not call them. Same rules as mdaw_engine.h otherwise. */
#ifndef MDAW_ENGINE_TEST_H
#define MDAW_ENGINE_TEST_H

#include "mdaw_engine.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Does what the close button of the device's plugin window does (the window hides; the
 * engine's timer deletes it later and sends MDAW_EVENT_EDITOR_CLOSED). MDAW_ERR_NOT_FOUND if
 * the device has no window. */
mdaw_status mdaw_test_editor_press_close(mdaw_engine* engine, uint64_t device);

/* 1 if the device has a plugin window (also one closed by the user and not deleted yet), with
 * whether it is visible and its token; 0 if not. visible and token may be NULL. */
int32_t mdaw_test_editor_state(mdaw_engine* engine, uint64_t device, int32_t* visible, uint64_t* token);

#ifdef __cplusplus
}
#endif
#endif
