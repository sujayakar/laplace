/// Minimal FFI bindings to QuickJS for evaluating JS code.

use core::ffi::{c_char, c_int, c_void};

// QuickJS value is a 128-bit struct (union + tag) on 64-bit platforms
#[repr(C)]
#[derive(Copy, Clone)]
pub struct JSValue {
    u: u64, // JSValueUnion — reinterpreted as needed
    tag: i64,
}

// Tags
const JS_TAG_EXCEPTION: i64 = 6;
const JS_TAG_FLOAT64: i64 = 8;
const JS_TAG_UNDEFINED: i64 = 3;
const JS_TAG_STRING: i64 = -7;

impl JSValue {
    fn new_float64(d: f64) -> Self {
        JSValue {
            u: d.to_bits(),
            tag: JS_TAG_FLOAT64,
        }
    }
}

type JSRuntime = c_void;
type JSContext = c_void;

// Eval flags
const JS_EVAL_TYPE_GLOBAL: c_int = 0;

// JSCFunctionEnum values
const JS_CFUNC_GENERIC: c_int = 0;

extern "C" {
    fn JS_NewRuntime() -> *mut JSRuntime;
    fn JS_FreeRuntime(rt: *mut JSRuntime);
    fn JS_NewContext(rt: *mut JSRuntime) -> *mut JSContext;
    fn JS_FreeContext(ctx: *mut JSContext);
    fn JS_Eval(
        ctx: *mut JSContext,
        input: *const c_char,
        input_len: usize,
        filename: *const c_char,
        eval_flags: c_int,
    ) -> JSValue;
    fn JS_FreeValue(ctx: *mut JSContext, val: JSValue);
    // QuickJS-NG exports JS_ToCStringLen2, not JS_ToCStringLen (which is static inline)
    fn JS_ToCStringLen2(
        ctx: *mut JSContext,
        plen: *mut usize,
        val: JSValue,
        cesu8: bool,
    ) -> *const c_char;
    fn JS_FreeCString(ctx: *mut JSContext, ptr: *const c_char);
    fn JS_GetException(ctx: *mut JSContext) -> JSValue;

    // For hooking console.log
    fn JS_GetGlobalObject(ctx: *mut JSContext) -> JSValue;
    fn JS_NewObject(ctx: *mut JSContext) -> JSValue;
    fn JS_SetPropertyStr(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: *const c_char,
        val: JSValue,
    ) -> c_int;
    // QuickJS-NG exports JS_NewCFunction2, not JS_NewCFunction (which is static inline)
    fn JS_NewCFunction2(
        ctx: *mut JSContext,
        func: Option<
            unsafe extern "C" fn(*mut JSContext, JSValue, c_int, *const JSValue) -> JSValue,
        >,
        name: *const c_char,
        length: c_int,
        cproto: c_int,
        magic: c_int,
    ) -> JSValue;
}

/// Wrapper for JS_ToCStringLen2 (matches the static inline JS_ToCStringLen)
unsafe fn js_to_cstring_len(
    ctx: *mut JSContext,
    plen: *mut usize,
    val: JSValue,
) -> *const c_char {
    JS_ToCStringLen2(ctx, plen, val, false)
}

/// Check if a JSValue is an exception (matches the static inline JS_IsException)
fn js_is_exception(val: JSValue) -> bool {
    val.tag == JS_TAG_EXCEPTION
}

/// Wrapper for JS_NewCFunction2 (matches the static inline JS_NewCFunction)
unsafe fn js_new_cfunction(
    ctx: *mut JSContext,
    func: Option<
        unsafe extern "C" fn(*mut JSContext, JSValue, c_int, *const JSValue) -> JSValue,
    >,
    name: *const c_char,
    length: c_int,
) -> JSValue {
    JS_NewCFunction2(ctx, func, name, length, JS_CFUNC_GENERIC, 0)
}

// ── Additional FFI for builtins ──────────────────────────────────

extern "C" {
    fn JS_GetPropertyStr(ctx: *mut JSContext, this_obj: JSValue, prop: *const c_char) -> JSValue;
    fn JS_ParseJSON(
        ctx: *mut JSContext,
        buf: *const c_char,
        buf_len: usize,
        filename: *const c_char,
    ) -> JSValue;
    // Available for future use: fn JS_NewStringLen(ctx, str, len) -> JSValue
}

// ── console.log implementation ──────────────────────────────────

unsafe extern "C" fn js_console_log(
    ctx: *mut JSContext,
    _this: JSValue,
    argc: c_int,
    argv: *const JSValue,
) -> JSValue {
    for i in 0..argc {
        if i > 0 {
            super::console_write(" ");
        }
        let val = *argv.add(i as usize);
        let mut len: usize = 0;
        let str_ptr = js_to_cstring_len(ctx, &mut len, val);
        if !str_ptr.is_null() {
            let bytes = core::slice::from_raw_parts(str_ptr as *const u8, len);
            super::console_write_bytes(bytes);
            JS_FreeCString(ctx, str_ptr);
        }
    }
    super::console_write("\n");

    // Return undefined
    JSValue {
        u: 0,
        tag: JS_TAG_UNDEFINED,
    }
}

// ── Date.now() → HC_TIME ────────────────────────────────────────

unsafe extern "C" fn js_date_now(
    _ctx: *mut JSContext,
    _this: JSValue,
    _argc: c_int,
    _argv: *const JSValue,
) -> JSValue {
    // HC_TIME returns nanoseconds; Date.now() returns milliseconds
    let ns = super::hypercall(convex_shared::HC_TIME, 0, 0, 0);
    JSValue::new_float64((ns / 1_000_000) as f64)
}

// ── Math.random() → HC_RANDOM ──────────────────────────────────

unsafe extern "C" fn js_math_random(
    _ctx: *mut JSContext,
    _this: JSValue,
    _argc: c_int,
    _argv: *const JSValue,
) -> JSValue {
    let r = super::hypercall(convex_shared::HC_RANDOM, 0, 0, 0);
    // Convert u64 to f64 in [0, 1)
    JSValue::new_float64((r >> 11) as f64 / ((1u64 << 53) as f64))
}

fn install_console(ctx: *mut JSContext) {
    unsafe {
        let global = JS_GetGlobalObject(ctx);
        let console = JS_NewObject(ctx);

        let log_fn = js_new_cfunction(
            ctx,
            Some(js_console_log),
            b"log\0".as_ptr() as *const c_char,
            1,
        );
        JS_SetPropertyStr(ctx, console, b"log\0".as_ptr() as *const c_char, log_fn);

        JS_SetPropertyStr(
            ctx,
            global,
            b"console\0".as_ptr() as *const c_char,
            console,
        );
        JS_FreeValue(ctx, global);
    }
}

// ── db.query() → HC_DB_READ ────────────────────────────────────

unsafe extern "C" fn js_db_query(
    ctx: *mut JSContext,
    _this: JSValue,
    argc: c_int,
    argv: *const JSValue,
) -> JSValue {
    use convex_shared::{HC_DB_READ, MAILBOX_GPA, MAILBOX_SIZE};

    if argc < 1 {
        super::console_write("db.query: missing collection argument\n");
        return JSValue { u: 0, tag: JS_TAG_UNDEFINED };
    }

    // Get the collection name string
    let mut len: usize = 0;
    let str_ptr = js_to_cstring_len(ctx, &mut len, *argv.add(0));
    if str_ptr.is_null() {
        return JSValue { u: 0, tag: JS_TAG_UNDEFINED };
    }

    // Write collection name to mailbox
    let mailbox = MAILBOX_GPA as *mut u8;
    if len >= MAILBOX_SIZE {
        JS_FreeCString(ctx, str_ptr);
        super::console_write("db.query: collection name too long\n");
        return JSValue { u: 0, tag: JS_TAG_UNDEFINED };
    }
    core::ptr::copy_nonoverlapping(str_ptr as *const u8, mailbox, len);
    *mailbox.add(len) = 0; // null-terminate
    JS_FreeCString(ctx, str_ptr);

    // HC_DB_READ: x1 = request length, returns response length in x0
    let resp_len = super::hypercall(HC_DB_READ, len as u64, 0, 0) as usize;

    if resp_len == 0 || resp_len >= MAILBOX_SIZE {
        // Return empty array
        return JS_ParseJSON(
            ctx,
            b"[]\0".as_ptr() as *const c_char,
            2,
            b"<db>\0".as_ptr() as *const c_char,
        );
    }

    // Parse JSON response from mailbox
    JS_ParseJSON(
        ctx,
        mailbox as *const c_char,
        resp_len,
        b"<db>\0".as_ptr() as *const c_char,
    )
}

fn install_deterministic_builtins(ctx: *mut JSContext) {
    unsafe {
        let global = JS_GetGlobalObject(ctx);

        // Override Date.now
        let date = JS_GetPropertyStr(ctx, global, b"Date\0".as_ptr() as *const c_char);
        let now_fn = js_new_cfunction(
            ctx,
            Some(js_date_now),
            b"now\0".as_ptr() as *const c_char,
            0,
        );
        JS_SetPropertyStr(ctx, date, b"now\0".as_ptr() as *const c_char, now_fn);
        JS_FreeValue(ctx, date);

        // Override Math.random
        let math = JS_GetPropertyStr(ctx, global, b"Math\0".as_ptr() as *const c_char);
        let random_fn = js_new_cfunction(
            ctx,
            Some(js_math_random),
            b"random\0".as_ptr() as *const c_char,
            0,
        );
        JS_SetPropertyStr(
            ctx,
            math,
            b"random\0".as_ptr() as *const c_char,
            random_fn,
        );
        JS_FreeValue(ctx, math);

        // Install db.query
        let db = JS_NewObject(ctx);
        let query_fn = js_new_cfunction(
            ctx,
            Some(js_db_query),
            b"query\0".as_ptr() as *const c_char,
            1,
        );
        JS_SetPropertyStr(ctx, db, b"query\0".as_ptr() as *const c_char, query_fn);
        JS_SetPropertyStr(
            ctx,
            global,
            b"db\0".as_ptr() as *const c_char,
            db,
        );

        JS_FreeValue(ctx, global);
    }
}

// ── Eval entry point ────────────────────────────────────────────

pub fn eval_js(js_code: &[u8]) -> u64 {
    unsafe {
        let rt = JS_NewRuntime();
        if rt.is_null() {
            super::console_write("Failed to create JS runtime\n");
            return 1;
        }

        let ctx = JS_NewContext(rt);
        if ctx.is_null() {
            super::console_write("Failed to create JS context\n");
            JS_FreeRuntime(rt);
            return 1;
        }

        // Install builtins
        install_console(ctx);
        install_deterministic_builtins(ctx);

        // Evaluate
        let filename = b"<input>\0";
        let result = JS_Eval(
            ctx,
            js_code.as_ptr() as *const c_char,
            js_code.len(),
            filename.as_ptr() as *const c_char,
            JS_EVAL_TYPE_GLOBAL,
        );

        let exit_code = if js_is_exception(result) {
            let exc = JS_GetException(ctx);
            let mut len: usize = 0;
            let str_ptr = js_to_cstring_len(ctx, &mut len, exc);
            if !str_ptr.is_null() {
                super::console_write("Exception: ");
                let bytes = core::slice::from_raw_parts(str_ptr as *const u8, len);
                super::console_write_bytes(bytes);
                super::console_write("\n");
                JS_FreeCString(ctx, str_ptr);
            }
            JS_FreeValue(ctx, exc);
            1
        } else {
            // Print result if it's a string
            if result.tag == JS_TAG_STRING {
                let mut len: usize = 0;
                let str_ptr = js_to_cstring_len(ctx, &mut len, result);
                if !str_ptr.is_null() {
                    let bytes = core::slice::from_raw_parts(str_ptr as *const u8, len);
                    super::console_write_bytes(bytes);
                    super::console_write("\n");
                    JS_FreeCString(ctx, str_ptr);
                }
            }
            0
        };

        JS_FreeValue(ctx, result);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);

        exit_code
    }
}
