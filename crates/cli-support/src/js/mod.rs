mod js2rust;
mod rust2js;

use crate::descriptor::VectorKind;
use crate::js::js2rust::Js2Rust;
use crate::js::rust2js::Rust2Js;
use crate::webidl::{AuxEnum, AuxExport, AuxExportKind, AuxImport, AuxStruct};
use crate::webidl::{JsImport, JsImportName, WasmBindgenAux, WebidlCustomSection};
use crate::{Bindgen, EncodeInto, OutputMode};
use failure::{bail, Error, ResultExt};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use walrus::{ExportId, ImportId, MemoryId, Module};

pub struct Context<'a> {
    globals: String,
    imports_post: String,
    typescript: String,
    exposed_globals: Option<HashSet<&'static str>>,
    required_internal_exports: HashSet<&'static str>,
    config: &'a Bindgen,
    pub module: &'a mut Module,
    bindings: WebidlCustomSection,

    /// A map representing the `import` statements we'll be generating in the JS
    /// glue. The key is the module we're importing from and the value is the
    /// list of identifier we're importing from the module, with optional
    /// renames for each identifier.
    js_imports: HashMap<String, Vec<(String, Option<String>)>>,

    /// A map of each wasm import and what JS to hook up to it.
    wasm_import_definitions: HashMap<ImportId, String>,

    /// A map from an import to the name we've locally imported it as.
    imported_names: HashMap<JsImportName, String>,

    /// A set of all defined identifiers through either exports or imports to
    /// the number of times they've been used, used to generate new
    /// identifiers.
    defined_identifiers: HashMap<String, usize>,

    exported_classes: Option<BTreeMap<String, ExportedClass>>,
    memory: MemoryId,

    /// A map of the name of npm dependencies we've loaded so far to the path
    /// they're defined in as well as their version specification.
    pub npm_dependencies: HashMap<String, (PathBuf, String)>,
}

#[derive(Default)]
pub struct ExportedClass {
    comments: String,
    contents: String,
    typescript: String,
    has_constructor: bool,
    wrap_needed: bool,
    /// Map from field name to type as a string plus whether it has a setter
    typescript_fields: HashMap<String, (String, bool)>,
}

const INITIAL_HEAP_VALUES: &[&str] = &["undefined", "null", "true", "false"];
// Must be kept in sync with `src/lib.rs` of the `wasm-bindgen` crate
const INITIAL_HEAP_OFFSET: usize = 32;

impl<'a> Context<'a> {
    pub fn new(module: &'a mut Module, config: &'a Bindgen) -> Result<Context<'a>, Error> {
        // Find the single memory, if there is one, and for ease of use in our
        // binding generation just inject one if there's not one already (and
        // we'll clean it up later if we end up not using it).
        let mut memories = module.memories.iter().map(|m| m.id());
        let memory = memories.next();
        if memories.next().is_some() {
            bail!("multiple memories currently not supported");
        }
        drop(memories);
        let memory = memory.unwrap_or_else(|| module.memories.add_local(false, 1, None));

        // And then we're good to go!
        Ok(Context {
            globals: String::new(),
            imports_post: String::new(),
            typescript: "/* tslint:disable */\n".to_string(),
            exposed_globals: Some(Default::default()),
            required_internal_exports: Default::default(),
            imported_names: Default::default(),
            js_imports: Default::default(),
            defined_identifiers: Default::default(),
            wasm_import_definitions: Default::default(),
            exported_classes: Some(Default::default()),
            config,
            bindings: *module
                .customs
                .delete_typed::<WebidlCustomSection>()
                .unwrap(),
            module,
            memory,
            npm_dependencies: Default::default(),
        })
    }

    fn should_write_global(&mut self, name: &'static str) -> bool {
        self.exposed_globals.as_mut().unwrap().insert(name)
    }

    fn export(
        &mut self,
        export_name: &str,
        contents: &str,
        comments: Option<String>,
    ) -> Result<(), Error> {
        let definition_name = generate_identifier(export_name, &mut self.defined_identifiers);
        if contents.starts_with("class") && definition_name != export_name {
            bail!("cannot shadow already defined class `{}`", export_name);
        }

        let contents = contents.trim();
        if let Some(ref c) = comments {
            self.globals.push_str(c);
            self.typescript.push_str(c);
        }
        let global = match self.config.mode {
            OutputMode::Node {
                experimental_modules: false,
            } => {
                if contents.starts_with("class") {
                    format!("{}\nmodule.exports.{1} = {1};\n", contents, export_name)
                } else {
                    format!("module.exports.{} = {};\n", export_name, contents)
                }
            }
            OutputMode::NoModules { .. } => {
                if contents.starts_with("class") {
                    format!("{}\n__exports.{1} = {1};\n", contents, export_name)
                } else {
                    format!("__exports.{} = {};\n", export_name, contents)
                }
            }
            OutputMode::Bundler { .. }
            | OutputMode::Node {
                experimental_modules: true,
            }
            | OutputMode::Web => {
                if contents.starts_with("function") {
                    let body = &contents[8..];
                    if export_name == definition_name {
                        format!("export function {}{}\n", export_name, body)
                    } else {
                        format!(
                            "function {}{}\nexport {{ {} as {} }};\n",
                            definition_name, body, definition_name, export_name,
                        )
                    }
                } else if contents.starts_with("class") {
                    assert_eq!(export_name, definition_name);
                    format!("export {}\n", contents)
                } else {
                    assert_eq!(export_name, definition_name);
                    format!("export const {} = {};\n", export_name, contents)
                }
            }
        };
        self.global(&global);
        Ok(())
    }

    fn require_internal_export(&mut self, name: &'static str) -> Result<(), Error> {
        if !self.required_internal_exports.insert(name) {
            return Ok(());
        }

        if self.module.exports.iter().any(|e| e.name == name) {
            return Ok(());
        }

        bail!(
            "the exported function `{}` is required to generate bindings \
             but it was not found in the wasm file, perhaps the `std` feature \
             of the `wasm-bindgen` crate needs to be enabled?",
            name
        );
    }

    pub fn finalize(&mut self, module_name: &str) -> Result<(String, String), Error> {
        // Finalize all bindings for JS classes. This is where we'll generate JS
        // glue for all classes as well as finish up a few final imports like
        // `__wrap` and such.
        self.write_classes()?;

        // We're almost done here, so we can delete any internal exports (like
        // `__wbindgen_malloc`) if none of our JS glue actually needed it.
        self.unexport_unused_internal_exports();

        // Handle the `start` function, if one was specified. If we're in a
        // --test mode (such as wasm-bindgen-test-runner) then we skip this
        // entirely. Otherwise we want to first add a start function to the
        // `start` section if one is specified.
        //
        // Note that once a start function is added, if any, we immediately
        // un-start it. This is done because we require that the JS glue
        // initializes first, so we execute wasm startup manually once the JS
        // glue is all in place.
        let mut needs_manual_start = false;
        if self.config.emit_start {
            needs_manual_start = self.unstart_start_function();
        }

        // After all we've done, especially
        // `unexport_unused_internal_exports()`, we probably have a bunch of
        // garbage in the module that's no longer necessary, so delete
        // everything that we don't actually need.
        walrus::passes::gc::run(self.module);

        // Cause any future calls to `should_write_global` to panic, making sure
        // we don't ask for items which we can no longer emit.
        drop(self.exposed_globals.take().unwrap());

        self.finalize_js(module_name, needs_manual_start)
    }

    /// Performs the task of actually generating the final JS module, be it
    /// `--target no-modules`, `--target web`, or for bundlers. This is the very
    /// last step performed in `finalize`.
    fn finalize_js(
        &mut self,
        module_name: &str,
        needs_manual_start: bool,
    ) -> Result<(String, String), Error> {
        let mut ts = self.typescript.clone();
        let mut js = String::new();
        if self.config.mode.no_modules() {
            js.push_str("(function() {\n");
        }

        // Depending on the output mode, generate necessary glue to actually
        // import the wasm file in one way or another.
        let mut init = (String::new(), String::new());
        let mut footer = String::new();
        let mut imports = self.js_import_header()?;
        match &self.config.mode {
            // In `--target no-modules` mode we need to both expose a name on
            // the global object as well as generate our own custom start
            // function.
            OutputMode::NoModules { global } => {
                js.push_str("const __exports = {};\n");
                js.push_str("let wasm;\n");
                init = self.gen_init(needs_manual_start);
                footer.push_str(&format!(
                    "self.{} = Object.assign(init, __exports);\n",
                    global
                ));
            }

            // With normal CommonJS node we need to defer requiring the wasm
            // until the end so most of our own exports are hooked up
            OutputMode::Node {
                experimental_modules: false,
            } => {
                js.push_str("let wasm;\n");

                for (id, js) in self.wasm_import_definitions.iter() {
                    let import = self.module.imports.get_mut(*id);
                    import.module = format!("./{}.js", module_name);
                    footer.push_str("\nmodule.exports.");
                    footer.push_str(&import.name);
                    footer.push_str(" = ");
                    footer.push_str(js.trim());
                    footer.push_str(";\n");
                }

                footer.push_str(&format!("wasm = require('./{}_bg');\n", module_name));
                if needs_manual_start {
                    footer.push_str("wasm.__wbindgen_start();\n");
                }
            }

            // With Bundlers and modern ES6 support in Node we can simply import
            // the wasm file as if it were an ES module and let the
            // bundler/runtime take care of it.
            OutputMode::Bundler { .. }
            | OutputMode::Node {
                experimental_modules: true,
            } => {
                imports.push_str(&format!("import * as wasm from './{}_bg';\n", module_name));
                for (id, js) in self.wasm_import_definitions.iter() {
                    let import = self.module.imports.get_mut(*id);
                    import.module = format!("./{}.js", module_name);
                    footer.push_str("\nexport const ");
                    footer.push_str(&import.name);
                    footer.push_str(" = ");
                    footer.push_str(js.trim());
                    footer.push_str(";\n");
                }
                if needs_manual_start {
                    footer.push_str("\nwasm.__wbindgen_start();\n");
                }
            }

            // With a browser-native output we're generating an ES module, but
            // browsers don't support natively importing wasm right now so we
            // expose the same initialization function as `--target no-modules`
            // as the default export of the module.
            OutputMode::Web => {
                self.imports_post.push_str("let wasm;\n");
                init = self.gen_init(needs_manual_start);
                footer.push_str("export default init;\n");
            }
        }

        let (init_js, init_ts) = init;

        ts.push_str(&init_ts);

        // Emit all the JS for importing all our functionality
        assert!(
            !self.config.mode.uses_es_modules() || js.is_empty(),
            "ES modules require imports to be at the start of the file"
        );
        js.push_str(&imports);
        js.push_str("\n");
        js.push_str(&self.imports_post);
        js.push_str("\n");

        // Emit all our exports from this module
        js.push_str(&self.globals);
        js.push_str("\n");

        // Generate the initialization glue, if there was any
        js.push_str(&init_js);
        js.push_str("\n");
        js.push_str(&footer);
        js.push_str("\n");
        if self.config.mode.no_modules() {
            js.push_str("})();\n");
        }

        while js.contains("\n\n\n") {
            js = js.replace("\n\n\n", "\n\n");
        }

        Ok((js, ts))
    }

    fn js_import_header(&self) -> Result<String, Error> {
        let mut imports = String::new();
        match &self.config.mode {
            OutputMode::NoModules { .. } => {
                for (module, _items) in self.js_imports.iter() {
                    bail!(
                        "importing from `{}` isn't supported with `--target no-modules`",
                        module
                    );
                }
            }

            OutputMode::Node {
                experimental_modules: false,
            } => {
                for (module, items) in self.js_imports.iter() {
                    imports.push_str("const { ");
                    for (i, (item, rename)) in items.iter().enumerate() {
                        if i > 0 {
                            imports.push_str(", ");
                        }
                        imports.push_str(item);
                        if let Some(other) = rename {
                            imports.push_str(": ");
                            imports.push_str(other)
                        }
                    }
                    imports.push_str(" } = require(String.raw`");
                    imports.push_str(module);
                    imports.push_str("`);\n");
                }
            }

            OutputMode::Bundler { .. }
            | OutputMode::Node {
                experimental_modules: true,
            }
            | OutputMode::Web => {
                for (module, items) in self.js_imports.iter() {
                    imports.push_str("import { ");
                    for (i, (item, rename)) in items.iter().enumerate() {
                        if i > 0 {
                            imports.push_str(", ");
                        }
                        imports.push_str(item);
                        if let Some(other) = rename {
                            imports.push_str(" as ");
                            imports.push_str(other)
                        }
                    }
                    imports.push_str(" } from '");
                    imports.push_str(module);
                    imports.push_str("';\n");
                }
            }
        }
        Ok(imports)
    }

    fn ts_for_init_fn(has_memory: bool) -> String {
        let (memory_doc, memory_param) = if has_memory {
            (
                "* @param {WebAssembly.Memory} maybe_memory\n",
                ", maybe_memory: WebAssembly.Memory",
            )
        } else {
            ("", "")
        };
        format!(
            "\n\
            /**\n\
            * If `module_or_path` is {{RequestInfo}}, makes a request and\n\
            * for everything else, calls `WebAssembly.instantiate` directly.\n\
            *\n\
            * @param {{RequestInfo | BufferSource | WebAssembly.Module}} module_or_path\n\
            {}\
            *\n\
            * @returns {{Promise<any>}}\n\
            */\n\
            export default function init \
                (module_or_path: RequestInfo | BufferSource | WebAssembly.Module{}): Promise<any>;
        ",
            memory_doc, memory_param
        )
    }

    fn gen_init(&mut self, needs_manual_start: bool) -> (String, String) {
        let mem = self.module.memories.get(self.memory);
        let (init_memory1, init_memory2) = if let Some(id) = mem.import {
            self.module.imports.get_mut(id).module = "wbg".to_string();
            let mut memory = String::from("new WebAssembly.Memory({");
            memory.push_str(&format!("initial:{}", mem.initial));
            if let Some(max) = mem.maximum {
                memory.push_str(&format!(",maximum:{}", max));
            }
            if mem.shared {
                memory.push_str(",shared:true");
            }
            memory.push_str("})");
            self.imports_post.push_str("let memory;\n");
            (
                format!("memory = imports.wbg.memory = maybe_memory;"),
                format!("memory = imports.wbg.memory = {};", memory),
            )
        } else {
            (String::new(), String::new())
        };
        let init_memory_arg = if mem.import.is_some() {
            ", maybe_memory"
        } else {
            ""
        };
        let ts = Self::ts_for_init_fn(mem.import.is_some());

        // Initialize the `imports` object for all import definitions that we're
        // directed to wire up.
        let mut imports_init = String::new();
        let module_name = "wbg";
        if self.wasm_import_definitions.len() > 0 {
            imports_init.push_str("imports.");
            imports_init.push_str(module_name);
            imports_init.push_str(" = {};\n");
        }
        for (id, js) in self.wasm_import_definitions.iter() {
            let import = self.module.imports.get_mut(*id);
            import.module = module_name.to_string();
            imports_init.push_str("imports.");
            imports_init.push_str(module_name);
            imports_init.push_str(".");
            imports_init.push_str(&import.name);
            imports_init.push_str(" = ");
            imports_init.push_str(js.trim());
            imports_init.push_str(";\n");
        }

        let js = format!(
            "\
                function init(module{init_memory_arg}) {{
                    let result;
                    const imports = {{}};
                    {imports_init}
                    if (module instanceof URL || typeof module === 'string' || module instanceof Request) {{
                        {init_memory2}
                        const response = fetch(module);
                        if (typeof WebAssembly.instantiateStreaming === 'function') {{
                            result = WebAssembly.instantiateStreaming(response, imports)
                                .catch(e => {{
                                    console.warn(\"`WebAssembly.instantiateStreaming` failed. Assuming this is \
                                                    because your server does not serve wasm with \
                                                    `application/wasm` MIME type. Falling back to \
                                                    `WebAssembly.instantiate` which is slower. Original \
                                                    error:\\n\", e);
                                    return response
                                        .then(r => r.arrayBuffer())
                                        .then(bytes => WebAssembly.instantiate(bytes, imports));
                                }});
                        }} else {{
                            result = response
                                .then(r => r.arrayBuffer())
                                .then(bytes => WebAssembly.instantiate(bytes, imports));
                        }}
                    }} else {{
                        {init_memory1}
                        result = WebAssembly.instantiate(module, imports)
                            .then(result => {{
                                if (result instanceof WebAssembly.Instance) {{
                                    return {{ instance: result, module }};
                                }} else {{
                                    return result;
                                }}
                            }});
                    }}
                    return result.then(({{instance, module}}) => {{
                        wasm = instance.exports;
                        init.__wbindgen_wasm_module = module;
                        {start}
                        return wasm;
                    }});
                }}
            ",
            init_memory_arg = init_memory_arg,
            init_memory1 = init_memory1,
            init_memory2 = init_memory2,
            start = if needs_manual_start {
                "wasm.__wbindgen_start();"
            } else {
                ""
            },
            imports_init = imports_init,
        );

        (js, ts)
    }

    fn write_classes(&mut self) -> Result<(), Error> {
        for (class, exports) in self.exported_classes.take().unwrap() {
            self.write_class(&class, &exports)?;
        }
        Ok(())
    }

    fn write_class(&mut self, name: &str, class: &ExportedClass) -> Result<(), Error> {
        let mut dst = format!("class {} {{\n", name);
        let mut ts_dst = format!("export {}", dst);

        if self.config.debug && !class.has_constructor {
            dst.push_str(
                "
                    constructor() {
                        throw new Error('cannot invoke `new` directly');
                    }
                ",
            );
        }

        if class.wrap_needed {
            dst.push_str(&format!(
                "
                static __wrap(ptr) {{
                    const obj = Object.create({}.prototype);
                    obj.ptr = ptr;
                    {}
                    return obj;
                }}
                ",
                name,
                if self.config.weak_refs {
                    format!("{}FinalizationGroup.register(obj, obj.ptr, obj.ptr);", name)
                } else {
                    String::new()
                },
            ));
        }

        self.global(&format!(
            "
            function free{}(ptr) {{
                wasm.{}(ptr);
            }}
            ",
            name,
            wasm_bindgen_shared::free_function(&name)
        ));

        if self.config.weak_refs {
            self.global(&format!(
                "
                const {}FinalizationGroup = new FinalizationGroup((items) => {{
                    for (const ptr of items) {{
                        free{}(ptr);
                    }}
                }});
                ",
                name, name,
            ));
        }

        dst.push_str(&format!(
            "
            free() {{
                const ptr = this.ptr;
                this.ptr = 0;
                {}
                free{}(ptr);
            }}
            ",
            if self.config.weak_refs {
                format!("{}FinalizationGroup.unregister(ptr);", name)
            } else {
                String::new()
            },
            name,
        ));
        ts_dst.push_str("  free(): void;");
        dst.push_str(&class.contents);
        ts_dst.push_str(&class.typescript);

        let mut fields = class.typescript_fields.keys().collect::<Vec<_>>();
        fields.sort(); // make sure we have deterministic output
        for name in fields {
            let (ty, readonly) = &class.typescript_fields[name];
            if *readonly {
                ts_dst.push_str("readonly ");
            }
            ts_dst.push_str(name);
            ts_dst.push_str(": ");
            ts_dst.push_str(ty);
            ts_dst.push_str(";\n");
        }
        dst.push_str("}\n");
        ts_dst.push_str("}\n");

        self.export(&name, &dst, Some(class.comments.clone()))?;
        self.typescript.push_str(&ts_dst);

        Ok(())
    }

    fn unexport_unused_internal_exports(&mut self) {
        let mut to_remove = Vec::new();
        for export in self.module.exports.iter() {
            match export.name.as_str() {
                // Otherwise only consider our special exports, which all start
                // with the same prefix which hopefully only we're using.
                n if n.starts_with("__wbindgen") => {
                    if !self.required_internal_exports.contains(n) {
                        to_remove.push(export.id());
                    }
                }
                _ => {}
            }
        }
        for id in to_remove {
            self.module.exports.delete(id);
        }
    }

    fn expose_does_not_exist(&mut self) {
        if !self.should_write_global("does_not_exist") {
            return;
        }
        self.global(
            "
                function doesNotExist() {
                    throw new Error('imported function or type does not exist');
                }
            ",
        );
    }

    fn expose_drop_ref(&mut self) {
        if !self.should_write_global("drop_ref") {
            return;
        }
        self.expose_global_heap();
        self.expose_global_heap_next();

        // Note that here we check if `idx` shouldn't actually be dropped. This
        // is due to the fact that `JsValue::null()` and friends can be passed
        // by value to JS where we'll automatically call this method. Those
        // constants, however, cannot be dropped. See #1054 for removing this
        // branch.
        //
        // Otherwise the free operation here is pretty simple, just appending to
        // the linked list of heap slots that are free.
        self.global(&format!(
            "
            function dropObject(idx) {{
                if (idx < {}) return;
                heap[idx] = heap_next;
                heap_next = idx;
            }}
            ",
            INITIAL_HEAP_OFFSET + INITIAL_HEAP_VALUES.len(),
        ));
    }

    fn expose_global_heap(&mut self) {
        if !self.should_write_global("heap") {
            return;
        }
        assert!(!self.config.anyref);
        self.global(&format!("const heap = new Array({});", INITIAL_HEAP_OFFSET));
        self.global("heap.fill(undefined);");
        self.global(&format!("heap.push({});", INITIAL_HEAP_VALUES.join(", ")));
    }

    fn expose_global_heap_next(&mut self) {
        if !self.should_write_global("heap_next") {
            return;
        }
        self.expose_global_heap();
        self.global("let heap_next = heap.length;");
    }

    fn expose_get_object(&mut self) {
        if !self.should_write_global("get_object") {
            return;
        }
        self.expose_global_heap();

        // Accessing a heap object is just a simple index operation due to how
        // the stack/heap are laid out.
        self.global("function getObject(idx) { return heap[idx]; }");
    }

    fn expose_assert_num(&mut self) {
        if !self.should_write_global("assert_num") {
            return;
        }
        self.global(&format!(
            "
            function _assertNum(n) {{
                if (typeof(n) !== 'number') throw new Error('expected a number argument');
            }}
            "
        ));
    }

    fn expose_assert_bool(&mut self) {
        if !self.should_write_global("assert_bool") {
            return;
        }
        self.global(&format!(
            "
            function _assertBoolean(n) {{
                if (typeof(n) !== 'boolean') {{
                    throw new Error('expected a boolean argument');
                }}
            }}
            "
        ));
    }

    fn expose_wasm_vector_len(&mut self) {
        if !self.should_write_global("wasm_vector_len") {
            return;
        }
        self.global("let WASM_VECTOR_LEN = 0;");
    }

    fn expose_pass_string_to_wasm(&mut self) -> Result<(), Error> {
        if !self.should_write_global("pass_string_to_wasm") {
            return Ok(());
        }
        self.require_internal_export("__wbindgen_malloc")?;
        self.expose_wasm_vector_len();
        let debug = if self.config.debug {
            "
                if (typeof(arg) !== 'string') throw new Error('expected a string argument');
            "
        } else {
            ""
        };

        // If we are targeting Node.js, it doesn't have `encodeInto` yet
        // but it does have `Buffer::write` which has similar semantics but
        // doesn't require creating intermediate view using `subarray`
        // and also has `Buffer::byteLength` to calculate size upfront.
        if self.config.mode.nodejs() {
            self.expose_node_buffer_memory();

            self.global(&format!(
                "
                    function passStringToWasm(arg) {{
                        {}
                        const size = Buffer.byteLength(arg);
                        const ptr = wasm.__wbindgen_malloc(size);
                        getNodeBufferMemory().write(arg, ptr, size);
                        WASM_VECTOR_LEN = size;
                        return ptr;
                    }}
                ",
                debug,
            ));

            return Ok(());
        }

        self.expose_text_encoder()?;
        self.expose_uint8_memory();

        // A fast path that directly writes char codes into WASM memory as long
        // as it finds only ASCII characters.
        //
        // This is much faster for common ASCII strings because it can avoid
        // calling out into C++ TextEncoder code.
        //
        // This might be not very intuitive, but such calls are usually more
        // expensive in mainstream engines than staying in the JS, and
        // charCodeAt on ASCII strings is usually optimised to raw bytes.
        let start_encoding_as_ascii = format!(
            "
                {}
                let size = arg.length;
                let ptr = wasm.__wbindgen_malloc(size);
                let offset = 0;
                {{
                    const mem = getUint8Memory();
                    for (; offset < arg.length; offset++) {{
                        const code = arg.charCodeAt(offset);
                        if (code > 0x7F) break;
                        mem[ptr + offset] = code;
                    }}
                }}
            ",
            debug
        );

        // The first implementation we have for this is to use
        // `TextEncoder#encode` which has been around for quite some time.
        let use_encode = format!(
            "
                {}
                if (offset !== arg.length) {{
                    const buf = cachedTextEncoder.encode(arg.slice(offset));
                    ptr = wasm.__wbindgen_realloc(ptr, size, size = offset + buf.length);
                    getUint8Memory().set(buf, ptr + offset);
                    offset += buf.length;
                }}
                WASM_VECTOR_LEN = offset;
                return ptr;
            ",
            start_encoding_as_ascii
        );

        // Another possibility is to use `TextEncoder#encodeInto` which is much
        // newer and isn't implemented everywhere yet. It's more efficient,
        // however, becaues it allows us to elide an intermediate allocation.
        let use_encode_into = format!(
            "
                {}
                if (offset !== arg.length) {{
                    arg = arg.slice(offset);
                    ptr = wasm.__wbindgen_realloc(ptr, size, size = offset + arg.length * 3);
                    const view = getUint8Memory().subarray(ptr + offset, ptr + size);
                    const ret = cachedTextEncoder.encodeInto(arg, view);
                    {}
                    offset += ret.written;
                }}
                WASM_VECTOR_LEN = offset;
                return ptr;
            ",
            start_encoding_as_ascii,
            if self.config.debug {
                "if (ret.read != arg.length) throw new Error('failed to pass whole string');"
            } else {
                ""
            },
        );

        // Looks like `encodeInto` doesn't currently work when the memory passed
        // in is backed by a `SharedArrayBuffer`, so force usage of `encode` if
        // a `SharedArrayBuffer` is in use.
        let shared = self.module.memories.get(self.memory).shared;

        match self.config.encode_into {
            EncodeInto::Always if !shared => {
                self.require_internal_export("__wbindgen_realloc")?;
                self.global(&format!(
                    "function passStringToWasm(arg) {{ {} }}",
                    use_encode_into,
                ));
            }
            EncodeInto::Test if !shared => {
                self.require_internal_export("__wbindgen_realloc")?;
                self.global(&format!(
                    "
                        let passStringToWasm;
                        if (typeof cachedTextEncoder.encodeInto === 'function') {{
                            passStringToWasm = function(arg) {{ {} }};
                        }} else {{
                            passStringToWasm = function(arg) {{ {} }};
                        }}
                    ",
                    use_encode_into, use_encode,
                ));
            }
            _ => {
                self.global(&format!(
                    "function passStringToWasm(arg) {{ {} }}",
                    use_encode,
                ));
            }
        }
        Ok(())
    }

    fn expose_pass_array8_to_wasm(&mut self) -> Result<(), Error> {
        self.expose_uint8_memory();
        self.pass_array_to_wasm("passArray8ToWasm", "getUint8Memory", 1)
    }

    fn expose_pass_array16_to_wasm(&mut self) -> Result<(), Error> {
        self.expose_uint16_memory();
        self.pass_array_to_wasm("passArray16ToWasm", "getUint16Memory", 2)
    }

    fn expose_pass_array32_to_wasm(&mut self) -> Result<(), Error> {
        self.expose_uint32_memory();
        self.pass_array_to_wasm("passArray32ToWasm", "getUint32Memory", 4)
    }

    fn expose_pass_array64_to_wasm(&mut self) -> Result<(), Error> {
        self.expose_uint64_memory();
        self.pass_array_to_wasm("passArray64ToWasm", "getUint64Memory", 8)
    }

    fn expose_pass_array_f32_to_wasm(&mut self) -> Result<(), Error> {
        self.expose_f32_memory();
        self.pass_array_to_wasm("passArrayF32ToWasm", "getFloat32Memory", 4)
    }

    fn expose_pass_array_f64_to_wasm(&mut self) -> Result<(), Error> {
        self.expose_f64_memory();
        self.pass_array_to_wasm("passArrayF64ToWasm", "getFloat64Memory", 8)
    }

    fn expose_pass_array_jsvalue_to_wasm(&mut self) -> Result<(), Error> {
        if !self.should_write_global("pass_array_jsvalue") {
            return Ok(());
        }
        self.require_internal_export("__wbindgen_malloc")?;
        self.expose_uint32_memory();
        self.expose_wasm_vector_len();
        if self.config.anyref {
            // TODO: using `addToAnyrefTable` goes back and forth between wasm
            // and JS a lot, we should have a bulk operation for this.
            self.expose_add_to_anyref_table()?;
            self.global(
                "
                function passArrayJsValueToWasm(array) {
                    const ptr = wasm.__wbindgen_malloc(array.length * 4);
                    const mem = getUint32Memory();
                    for (let i = 0; i < array.length; i++) {
                        mem[ptr / 4 + i] = addToAnyrefTable(array[i]);
                    }
                    WASM_VECTOR_LEN = array.length;
                    return ptr;
                }
            ",
            );
        } else {
            self.expose_add_heap_object();
            self.global(
                "
                function passArrayJsValueToWasm(array) {
                    const ptr = wasm.__wbindgen_malloc(array.length * 4);
                    const mem = getUint32Memory();
                    for (let i = 0; i < array.length; i++) {
                        mem[ptr / 4 + i] = addHeapObject(array[i]);
                    }
                    WASM_VECTOR_LEN = array.length;
                    return ptr;
                }

            ",
            );
        }
        Ok(())
    }

    fn pass_array_to_wasm(
        &mut self,
        name: &'static str,
        delegate: &str,
        size: usize,
    ) -> Result<(), Error> {
        if !self.should_write_global(name) {
            return Ok(());
        }
        self.require_internal_export("__wbindgen_malloc")?;
        self.expose_wasm_vector_len();
        self.global(&format!(
            "
            function {}(arg) {{
                const ptr = wasm.__wbindgen_malloc(arg.length * {size});
                {}().set(arg, ptr / {size});
                WASM_VECTOR_LEN = arg.length;
                return ptr;
            }}
            ",
            name,
            delegate,
            size = size
        ));
        Ok(())
    }

    fn expose_text_encoder(&mut self) -> Result<(), Error> {
        if !self.should_write_global("text_encoder") {
            return Ok(());
        }
        self.expose_text_processor("TextEncoder")
    }

    fn expose_text_decoder(&mut self) -> Result<(), Error> {
        if !self.should_write_global("text_decoder") {
            return Ok(());
        }
        self.expose_text_processor("TextDecoder")?;
        Ok(())
    }

    fn expose_text_processor(&mut self, s: &str) -> Result<(), Error> {
        if self.config.mode.nodejs() {
            let name = self.import_name(&JsImport {
                name: JsImportName::Module {
                    module: "util".to_string(),
                    name: s.to_string(),
                },
                fields: Vec::new(),
            })?;
            self.global(&format!("let cached{} = new {}('utf-8');", s, name));
        } else if !self.config.mode.always_run_in_browser() {
            self.global(&format!(
                "
                    const l{0} = typeof {0} === 'undefined' ? \
                        require('util').{0} : {0};\
                ",
                s
            ));
            self.global(&format!("let cached{0} = new l{0}('utf-8');", s));
        } else {
            self.global(&format!("let cached{0} = new {0}('utf-8');", s));
        }
        Ok(())
    }

    fn expose_get_string_from_wasm(&mut self) -> Result<(), Error> {
        if !self.should_write_global("get_string_from_wasm") {
            return Ok(());
        }
        self.expose_text_decoder()?;
        self.expose_uint8_memory();

        // Typically we try to give a raw view of memory out to `TextDecoder` to
        // avoid copying too much data. If, however, a `SharedArrayBuffer` is
        // being used it looks like that is rejected by `TextDecoder` or
        // otherwise doesn't work with it. When we detect a shared situation we
        // use `slice` which creates a new array instead of `subarray` which
        // creates just a view. That way in shared mode we copy more data but in
        // non-shared mode there's no need to copy the data except for the
        // string itself.
        let is_shared = self.module.memories.get(self.memory).shared;
        let method = if is_shared { "slice" } else { "subarray" };

        self.global(&format!(
            "
            function getStringFromWasm(ptr, len) {{
                return cachedTextDecoder.decode(getUint8Memory().{}(ptr, ptr + len));
            }}
        ",
            method
        ));
        Ok(())
    }

    fn expose_get_array_js_value_from_wasm(&mut self) -> Result<(), Error> {
        if !self.should_write_global("get_array_js_value_from_wasm") {
            return Ok(());
        }
        self.expose_uint32_memory();
        if self.config.anyref {
            self.expose_anyref_table();
            self.global(
                "
                function getArrayJsValueFromWasm(ptr, len) {
                    const mem = getUint32Memory();
                    const slice = mem.subarray(ptr / 4, ptr / 4 + len);
                    const result = [];
                    for (let i = 0; i < slice.length; i++) {
                        result.push(wasm.__wbg_anyref_table.get(slice[i]));
                    }
                    wasm.__wbindgen_drop_anyref_slice(ptr, len);
                    return result;
                }
                ",
            );
            self.require_internal_export("__wbindgen_drop_anyref_slice")?;
        } else {
            self.expose_take_object();
            self.global(
                "
                function getArrayJsValueFromWasm(ptr, len) {
                    const mem = getUint32Memory();
                    const slice = mem.subarray(ptr / 4, ptr / 4 + len);
                    const result = [];
                    for (let i = 0; i < slice.length; i++) {
                        result.push(takeObject(slice[i]));
                    }
                    return result;
                }
                ",
            );
        }
        Ok(())
    }

    fn expose_get_array_i8_from_wasm(&mut self) {
        self.expose_int8_memory();
        self.arrayget("getArrayI8FromWasm", "getInt8Memory", 1);
    }

    fn expose_get_array_u8_from_wasm(&mut self) {
        self.expose_uint8_memory();
        self.arrayget("getArrayU8FromWasm", "getUint8Memory", 1);
    }

    fn expose_get_clamped_array_u8_from_wasm(&mut self) {
        self.expose_clamped_uint8_memory();
        self.arrayget("getClampedArrayU8FromWasm", "getUint8ClampedMemory", 1);
    }

    fn expose_get_array_i16_from_wasm(&mut self) {
        self.expose_int16_memory();
        self.arrayget("getArrayI16FromWasm", "getInt16Memory", 2);
    }

    fn expose_get_array_u16_from_wasm(&mut self) {
        self.expose_uint16_memory();
        self.arrayget("getArrayU16FromWasm", "getUint16Memory", 2);
    }

    fn expose_get_array_i32_from_wasm(&mut self) {
        self.expose_int32_memory();
        self.arrayget("getArrayI32FromWasm", "getInt32Memory", 4);
    }

    fn expose_get_array_u32_from_wasm(&mut self) {
        self.expose_uint32_memory();
        self.arrayget("getArrayU32FromWasm", "getUint32Memory", 4);
    }

    fn expose_get_array_i64_from_wasm(&mut self) {
        self.expose_int64_memory();
        self.arrayget("getArrayI64FromWasm", "getInt64Memory", 8);
    }

    fn expose_get_array_u64_from_wasm(&mut self) {
        self.expose_uint64_memory();
        self.arrayget("getArrayU64FromWasm", "getUint64Memory", 8);
    }

    fn expose_get_array_f32_from_wasm(&mut self) {
        self.expose_f32_memory();
        self.arrayget("getArrayF32FromWasm", "getFloat32Memory", 4);
    }

    fn expose_get_array_f64_from_wasm(&mut self) {
        self.expose_f64_memory();
        self.arrayget("getArrayF64FromWasm", "getFloat64Memory", 8);
    }

    fn arrayget(&mut self, name: &'static str, mem: &'static str, size: usize) {
        if !self.should_write_global(name) {
            return;
        }
        self.global(&format!(
            "
            function {name}(ptr, len) {{
                return {mem}().subarray(ptr / {size}, ptr / {size} + len);
            }}
            ",
            name = name,
            mem = mem,
            size = size,
        ));
    }

    fn expose_node_buffer_memory(&mut self) {
        self.memview("getNodeBufferMemory", "Buffer.from");
    }

    fn expose_int8_memory(&mut self) {
        self.memview("getInt8Memory", "new Int8Array");
    }

    fn expose_uint8_memory(&mut self) {
        self.memview("getUint8Memory", "new Uint8Array");
    }

    fn expose_clamped_uint8_memory(&mut self) {
        self.memview("getUint8ClampedMemory", "new Uint8ClampedArray");
    }

    fn expose_int16_memory(&mut self) {
        self.memview("getInt16Memory", "new Int16Array");
    }

    fn expose_uint16_memory(&mut self) {
        self.memview("getUint16Memory", "new Uint16Array");
    }

    fn expose_int32_memory(&mut self) {
        self.memview("getInt32Memory", "new Int32Array");
    }

    fn expose_uint32_memory(&mut self) {
        self.memview("getUint32Memory", "new Uint32Array");
    }

    fn expose_int64_memory(&mut self) {
        self.memview("getInt64Memory", "new BigInt64Array");
    }

    fn expose_uint64_memory(&mut self) {
        self.memview("getUint64Memory", "new BigUint64Array");
    }

    fn expose_f32_memory(&mut self) {
        self.memview("getFloat32Memory", "new Float32Array");
    }

    fn expose_f64_memory(&mut self) {
        self.memview("getFloat64Memory", "new Float64Array");
    }

    fn memview_function(&mut self, t: VectorKind) -> &'static str {
        match t {
            VectorKind::String => {
                self.expose_uint8_memory();
                "getUint8Memory"
            }
            VectorKind::I8 => {
                self.expose_int8_memory();
                "getInt8Memory"
            }
            VectorKind::U8 => {
                self.expose_uint8_memory();
                "getUint8Memory"
            }
            VectorKind::ClampedU8 => {
                self.expose_clamped_uint8_memory();
                "getUint8ClampedMemory"
            }
            VectorKind::I16 => {
                self.expose_int16_memory();
                "getInt16Memory"
            }
            VectorKind::U16 => {
                self.expose_uint16_memory();
                "getUint16Memory"
            }
            VectorKind::I32 => {
                self.expose_int32_memory();
                "getInt32Memory"
            }
            VectorKind::U32 => {
                self.expose_uint32_memory();
                "getUint32Memory"
            }
            VectorKind::I64 => {
                self.expose_int64_memory();
                "getInt64Memory"
            }
            VectorKind::U64 => {
                self.expose_uint64_memory();
                "getUint64Memory"
            }
            VectorKind::F32 => {
                self.expose_f32_memory();
                "getFloat32Memory"
            }
            VectorKind::F64 => {
                self.expose_f64_memory();
                "getFloat64Memory"
            }
            VectorKind::Anyref => {
                self.expose_uint32_memory();
                "getUint32Memory"
            }
        }
    }

    fn memview(&mut self, name: &'static str, js: &str) {
        if !self.should_write_global(name) {
            return;
        }
        let mem = self.memory();
        self.global(&format!(
            "
            let cache{name} = null;
            function {name}() {{
                if (cache{name} === null || cache{name}.buffer !== {mem}.buffer) {{
                    cache{name} = {js}({mem}.buffer);
                }}
                return cache{name};
            }}
            ",
            name = name,
            js = js,
            mem = mem,
        ));
    }

    fn expose_assert_class(&mut self) {
        if !self.should_write_global("assert_class") {
            return;
        }
        self.global(
            "
            function _assertClass(instance, klass) {
                if (!(instance instanceof klass)) {
                    throw new Error(`expected instance of ${klass.name}`);
                }
                return instance.ptr;
            }
            ",
        );
    }

    fn expose_global_stack_pointer(&mut self) {
        if !self.should_write_global("stack_pointer") {
            return;
        }
        self.global(&format!("let stack_pointer = {};", INITIAL_HEAP_OFFSET));
    }

    fn expose_borrowed_objects(&mut self) {
        if !self.should_write_global("borrowed_objects") {
            return;
        }
        self.expose_global_heap();
        self.expose_global_stack_pointer();
        // Our `stack_pointer` points to where we should start writing stack
        // objects, and the `stack_pointer` is incremented in a `finally` block
        // after executing this. Once we've reserved stack space we write the
        // value. Eventually underflow will throw an exception, but JS sort of
        // just handles it today...
        self.global(
            "
            function addBorrowedObject(obj) {
                if (stack_pointer == 1) throw new Error('out of js stack');
                heap[--stack_pointer] = obj;
                return stack_pointer;
            }
            ",
        );
    }

    fn expose_take_object(&mut self) {
        if !self.should_write_global("take_object") {
            return;
        }
        self.expose_get_object();
        self.expose_drop_ref();
        self.global(
            "
            function takeObject(idx) {
                const ret = getObject(idx);
                dropObject(idx);
                return ret;
            }
            ",
        );
    }

    fn expose_add_heap_object(&mut self) {
        if !self.should_write_global("add_heap_object") {
            return;
        }
        self.expose_global_heap();
        self.expose_global_heap_next();
        let set_heap_next = if self.config.debug {
            String::from(
                "
                if (typeof(heap_next) !== 'number') throw new Error('corrupt heap');
                ",
            )
        } else {
            String::new()
        };

        // Allocating a slot on the heap first goes through the linked list
        // (starting at `heap_next`). Once that linked list is exhausted we'll
        // be pointing beyond the end of the array, at which point we'll reserve
        // one more slot and use that.
        self.global(&format!(
            "
            function addHeapObject(obj) {{
                if (heap_next === heap.length) heap.push(heap.length + 1);
                const idx = heap_next;
                heap_next = heap[idx];
                {}
                heap[idx] = obj;
                return idx;
            }}
            ",
            set_heap_next
        ));
    }

    fn expose_handle_error(&mut self) -> Result<(), Error> {
        if !self.should_write_global("handle_error") {
            return Ok(());
        }
        self.expose_uint32_memory();
        if self.config.anyref {
            self.expose_add_to_anyref_table()?;
            self.global(
                "
                function handleError(exnptr, e) {
                    const idx = addToAnyrefTable(e);
                    const view = getUint32Memory();
                    view[exnptr / 4] = 1;
                    view[exnptr / 4 + 1] = idx;
                }
                ",
            );
        } else {
            self.expose_add_heap_object();
            self.global(
                "
                function handleError(exnptr, e) {
                    const view = getUint32Memory();
                    view[exnptr / 4] = 1;
                    view[exnptr / 4 + 1] = addHeapObject(e);
                }
                ",
            );
        }
        Ok(())
    }

    fn pass_to_wasm_function(&mut self, t: VectorKind) -> Result<&'static str, Error> {
        let s = match t {
            VectorKind::String => {
                self.expose_pass_string_to_wasm()?;
                "passStringToWasm"
            }
            VectorKind::I8 | VectorKind::U8 | VectorKind::ClampedU8 => {
                self.expose_pass_array8_to_wasm()?;
                "passArray8ToWasm"
            }
            VectorKind::U16 | VectorKind::I16 => {
                self.expose_pass_array16_to_wasm()?;
                "passArray16ToWasm"
            }
            VectorKind::I32 | VectorKind::U32 => {
                self.expose_pass_array32_to_wasm()?;
                "passArray32ToWasm"
            }
            VectorKind::I64 | VectorKind::U64 => {
                self.expose_pass_array64_to_wasm()?;
                "passArray64ToWasm"
            }
            VectorKind::F32 => {
                self.expose_pass_array_f32_to_wasm()?;
                "passArrayF32ToWasm"
            }
            VectorKind::F64 => {
                self.expose_pass_array_f64_to_wasm()?;
                "passArrayF64ToWasm"
            }
            VectorKind::Anyref => {
                self.expose_pass_array_jsvalue_to_wasm()?;
                "passArrayJsValueToWasm"
            }
        };
        Ok(s)
    }

    fn expose_get_vector_from_wasm(&mut self, ty: VectorKind) -> Result<&'static str, Error> {
        Ok(match ty {
            VectorKind::String => {
                self.expose_get_string_from_wasm()?;
                "getStringFromWasm"
            }
            VectorKind::I8 => {
                self.expose_get_array_i8_from_wasm();
                "getArrayI8FromWasm"
            }
            VectorKind::U8 => {
                self.expose_get_array_u8_from_wasm();
                "getArrayU8FromWasm"
            }
            VectorKind::ClampedU8 => {
                self.expose_get_clamped_array_u8_from_wasm();
                "getClampedArrayU8FromWasm"
            }
            VectorKind::I16 => {
                self.expose_get_array_i16_from_wasm();
                "getArrayI16FromWasm"
            }
            VectorKind::U16 => {
                self.expose_get_array_u16_from_wasm();
                "getArrayU16FromWasm"
            }
            VectorKind::I32 => {
                self.expose_get_array_i32_from_wasm();
                "getArrayI32FromWasm"
            }
            VectorKind::U32 => {
                self.expose_get_array_u32_from_wasm();
                "getArrayU32FromWasm"
            }
            VectorKind::I64 => {
                self.expose_get_array_i64_from_wasm();
                "getArrayI64FromWasm"
            }
            VectorKind::U64 => {
                self.expose_get_array_u64_from_wasm();
                "getArrayU64FromWasm"
            }
            VectorKind::F32 => {
                self.expose_get_array_f32_from_wasm();
                "getArrayF32FromWasm"
            }
            VectorKind::F64 => {
                self.expose_get_array_f64_from_wasm();
                "getArrayF64FromWasm"
            }
            VectorKind::Anyref => {
                self.expose_get_array_js_value_from_wasm()?;
                "getArrayJsValueFromWasm"
            }
        })
    }

    fn expose_global_argument_ptr(&mut self) -> Result<(), Error> {
        if !self.should_write_global("global_argument_ptr") {
            return Ok(());
        }
        self.require_internal_export("__wbindgen_global_argument_ptr")?;
        self.global(
            "
            let cachedGlobalArgumentPtr = null;
            function globalArgumentPtr() {
                if (cachedGlobalArgumentPtr === null) {
                    cachedGlobalArgumentPtr = wasm.__wbindgen_global_argument_ptr();
                }
                return cachedGlobalArgumentPtr;
            }
            ",
        );
        Ok(())
    }

    fn expose_get_inherited_descriptor(&mut self) {
        if !self.should_write_global("get_inherited_descriptor") {
            return;
        }
        // It looks like while rare some browsers will move descriptors up the
        // property chain which runs the risk of breaking wasm-bindgen-generated
        // code because we're looking for precise descriptor functions rather
        // than relying on the prototype chain like most "normal JS" projects
        // do.
        //
        // As a result we have a small helper here which will walk the prototype
        // chain looking for a descriptor. For some more information on this see
        // #109
        self.global(
            "
            function GetOwnOrInheritedPropertyDescriptor(obj, id) {
              while (obj) {
                let desc = Object.getOwnPropertyDescriptor(obj, id);
                if (desc) return desc;
                obj = Object.getPrototypeOf(obj);
              }
              return {}
            }
            ",
        );
    }

    fn expose_u32_cvt_shim(&mut self) -> &'static str {
        let name = "u32CvtShim";
        if !self.should_write_global(name) {
            return name;
        }
        self.global(&format!("const {} = new Uint32Array(2);", name));
        name
    }

    fn expose_int64_cvt_shim(&mut self) -> &'static str {
        let name = "int64CvtShim";
        if !self.should_write_global(name) {
            return name;
        }
        let n = self.expose_u32_cvt_shim();
        self.global(&format!(
            "const {} = new BigInt64Array({}.buffer);",
            name, n
        ));
        name
    }

    fn expose_uint64_cvt_shim(&mut self) -> &'static str {
        let name = "uint64CvtShim";
        if !self.should_write_global(name) {
            return name;
        }
        let n = self.expose_u32_cvt_shim();
        self.global(&format!(
            "const {} = new BigUint64Array({}.buffer);",
            name, n
        ));
        name
    }

    fn expose_is_like_none(&mut self) {
        if !self.should_write_global("is_like_none") {
            return;
        }
        self.global(
            "
            function isLikeNone(x) {
                return x === undefined || x === null;
            }
        ",
        );
    }

    fn global(&mut self, s: &str) {
        let s = s.trim();

        // Ensure a blank line between adjacent items, and ensure everything is
        // terminated with a newline.
        while !self.globals.ends_with("\n\n\n") && !self.globals.ends_with("*/\n") {
            self.globals.push_str("\n");
        }
        self.globals.push_str(s);
        self.globals.push_str("\n");
    }

    fn memory(&mut self) -> &'static str {
        if self.module.memories.get(self.memory).import.is_some() {
            "memory"
        } else {
            "wasm.memory"
        }
    }

    fn require_class_wrap(&mut self, name: &str) {
        require_class(&mut self.exported_classes, name).wrap_needed = true;
    }

    fn import_name(&mut self, import: &JsImport) -> Result<String, Error> {
        if let Some(name) = self.imported_names.get(&import.name) {
            let mut name = name.clone();
            for field in import.fields.iter() {
                name.push_str(".");
                name.push_str(field);
            }
            return Ok(name.clone());
        }

        let js_imports = &mut self.js_imports;
        let mut add_module_import = |module: String, name: &str, actual: &str| {
            let rename = if name == actual {
                None
            } else {
                Some(actual.to_string())
            };
            js_imports
                .entry(module)
                .or_insert(Vec::new())
                .push((name.to_string(), rename));
        };

        let mut name = match &import.name {
            JsImportName::Module { module, name } => {
                let unique_name = generate_identifier(name, &mut self.defined_identifiers);
                add_module_import(module.clone(), name, &unique_name);
                unique_name
            }

            JsImportName::LocalModule { module, name } => {
                let unique_name = generate_identifier(name, &mut self.defined_identifiers);
                add_module_import(format!("./snippets/{}", module), name, &unique_name);
                unique_name
            }

            JsImportName::InlineJs {
                unique_crate_identifier,
                snippet_idx_in_crate,
                name,
            } => {
                let unique_name = generate_identifier(name, &mut self.defined_identifiers);
                let module = format!(
                    "./snippets/{}/inline{}.js",
                    unique_crate_identifier, snippet_idx_in_crate,
                );
                add_module_import(module, name, &unique_name);
                unique_name
            }

            JsImportName::VendorPrefixed { name, prefixes } => {
                self.imports_post.push_str("const l");
                self.imports_post.push_str(&name);
                self.imports_post.push_str(" = ");
                switch(&mut self.imports_post, name, "", prefixes);
                self.imports_post.push_str(";\n");

                fn switch(dst: &mut String, name: &str, prefix: &str, left: &[String]) {
                    if left.len() == 0 {
                        dst.push_str(prefix);
                        return dst.push_str(name);
                    }
                    dst.push_str("(typeof ");
                    dst.push_str(prefix);
                    dst.push_str(name);
                    dst.push_str(" !== 'undefined' ? ");
                    dst.push_str(prefix);
                    dst.push_str(name);
                    dst.push_str(" : ");
                    switch(dst, name, &left[0], &left[1..]);
                    dst.push_str(")");
                }
                format!("l{}", name)
            }

            JsImportName::Global { name } => {
                let unique_name = generate_identifier(name, &mut self.defined_identifiers);
                if unique_name != *name {
                    bail!("cannot import `{}` from two locations", name);
                }
                unique_name
            }
        };
        self.imported_names
            .insert(import.name.clone(), name.clone());

        // After we've got an actual name handle field projections
        for field in import.fields.iter() {
            name.push_str(".");
            name.push_str(field);
        }
        Ok(name)
    }

    /// If a start function is present, it removes it from the `start` section
    /// of the wasm module and then moves it to an exported function, named
    /// `__wbindgen_start`.
    fn unstart_start_function(&mut self) -> bool {
        let start = match self.module.start.take() {
            Some(id) => id,
            None => return false,
        };
        self.module.exports.add("__wbindgen_start", start);
        true
    }

    fn expose_anyref_table(&mut self) {
        assert!(self.config.anyref);
        if !self.should_write_global("anyref_table") {
            return;
        }
        let table = self
            .module
            .tables
            .iter()
            .find(|t| match t.kind {
                walrus::TableKind::Anyref(_) => true,
                _ => false,
            })
            .expect("failed to find anyref table in module")
            .id();
        self.module.exports.add("__wbg_anyref_table", table);
    }

    fn expose_add_to_anyref_table(&mut self) -> Result<(), Error> {
        assert!(self.config.anyref);
        if !self.should_write_global("add_to_anyref_table") {
            return Ok(());
        }
        self.expose_anyref_table();
        self.require_internal_export("__wbindgen_anyref_table_alloc")?;
        self.global(
            "
                function addToAnyrefTable(obj) {
                    const idx = wasm.__wbindgen_anyref_table_alloc();
                    wasm.__wbg_anyref_table.set(idx, obj);
                    return idx;
                }
            ",
        );

        Ok(())
    }

    fn take_object(&mut self, expr: &str) -> String {
        if self.config.anyref {
            expr.to_string()
        } else {
            self.expose_take_object();
            format!("takeObject({})", expr)
        }
    }

    fn get_object(&mut self, expr: &str) -> String {
        if self.config.anyref {
            expr.to_string()
        } else {
            self.expose_get_object();
            format!("getObject({})", expr)
        }
    }

    pub fn generate(&mut self, aux: &WasmBindgenAux) -> Result<(), Error> {
        for (id, export) in aux.export_map.iter() {
            self.generate_export(*id, export).with_context(|_| {
                format!(
                    "failed to generate bindings for Rust export `{}`",
                    export.debug_name,
                )
            })?;
        }
        for (id, import) in aux.import_map.iter() {
            let variadic = aux.imports_with_variadic.contains(&id);
            let catch = aux.imports_with_catch.contains(&id);
            self.generate_import(*id, import, variadic, catch)
                .with_context(|_| {
                    format!("failed to generate bindings for import `{:?}`", import,)
                })?;
        }
        for e in aux.enums.iter() {
            self.generate_enum(e)?;
        }

        for s in aux.structs.iter() {
            self.generate_struct(s)?;
        }

        self.typescript.push_str(&aux.extra_typescript);

        for path in aux.package_jsons.iter() {
            self.process_package_json(path)?;
        }

        Ok(())
    }

    fn generate_export(&mut self, id: ExportId, export: &AuxExport) -> Result<(), Error> {
        let wasm_name = self.module.exports.get(id).name.clone();
        let descriptor = self.bindings.exports[&id].clone();
        match &export.kind {
            AuxExportKind::Function(name) => {
                let (js, ts, js_doc) = Js2Rust::new(&name, self)
                    .process(&descriptor, &export.arg_names)?
                    .finish("function", &format!("wasm.{}", wasm_name));
                self.export(
                    &name,
                    &js,
                    Some(format_doc_comments(&export.comments, Some(js_doc))),
                )?;
                self.globals.push_str("\n");
                self.typescript.push_str("export ");
                self.typescript.push_str(&ts);
                self.typescript.push_str("\n");
            }
            AuxExportKind::Constructor(class) => {
                let (js, ts, raw_docs) = Js2Rust::new("constructor", self)
                    .constructor(Some(&class))
                    .process(&descriptor, &export.arg_names)?
                    .finish("", &format!("wasm.{}", wasm_name));
                let exported = require_class(&mut self.exported_classes, class);
                if exported.has_constructor {
                    bail!("found duplicate constructor for class `{}`", class);
                }
                exported.has_constructor = true;
                let docs = format_doc_comments(&export.comments, Some(raw_docs));
                exported.push(&docs, "constructor", "", &js, &ts);
            }
            AuxExportKind::Getter { class, field: name }
            | AuxExportKind::Setter { class, field: name }
            | AuxExportKind::StaticFunction { class, name }
            | AuxExportKind::Method { class, name, .. } => {
                let mut j2r = Js2Rust::new(name, self);
                match export.kind {
                    AuxExportKind::StaticFunction { .. } => {}
                    AuxExportKind::Method { consumed: true, .. } => {
                        j2r.method(true);
                    }
                    _ => {
                        j2r.method(false);
                    }
                }
                let (js, ts, raw_docs) = j2r
                    .process(&descriptor, &export.arg_names)?
                    .finish("", &format!("wasm.{}", wasm_name));
                let ret_ty = j2r.ret_ty.clone();
                let exported = require_class(&mut self.exported_classes, class);
                let docs = format_doc_comments(&export.comments, Some(raw_docs));
                match export.kind {
                    AuxExportKind::Getter { .. } => {
                        exported.push_field(&docs, name, &js, Some(&ret_ty), true);
                    }
                    AuxExportKind::Setter { .. } => {
                        exported.push_field(&docs, name, &js, None, false);
                    }
                    AuxExportKind::StaticFunction { .. } => {
                        exported.push(&docs, name, "static ", &js, &ts);
                    }
                    _ => {
                        exported.push(&docs, name, "", &js, &ts);
                    }
                }
            }
        }
        Ok(())
    }

    fn generate_import(
        &mut self,
        id: ImportId,
        import: &AuxImport,
        variadic: bool,
        catch: bool,
    ) -> Result<(), Error> {
        let signature = self.bindings.imports[&id].clone();
        let catch_and_rethrow = self.config.debug;
        let js = Rust2Js::new(self)
            .catch_and_rethrow(catch_and_rethrow)
            .catch(catch)
            .variadic(variadic)
            .process(&signature)?
            .finish(import)?;
        self.wasm_import_definitions.insert(id, js);
        Ok(())
    }

    fn generate_enum(&mut self, enum_: &AuxEnum) -> Result<(), Error> {
        let mut variants = String::new();

        self.typescript
            .push_str(&format!("export enum {} {{", enum_.name));
        for (name, value) in enum_.variants.iter() {
            variants.push_str(&format!("{}:{},", name, value));
            self.typescript.push_str(&format!("\n  {},", name));
        }
        self.typescript.push_str("\n}\n");
        self.export(
            &enum_.name,
            &format!("Object.freeze({{ {} }})", variants),
            Some(format_doc_comments(&enum_.comments, None)),
        )?;

        Ok(())
    }

    fn generate_struct(&mut self, struct_: &AuxStruct) -> Result<(), Error> {
        let class = require_class(&mut self.exported_classes, &struct_.name);
        class.comments = format_doc_comments(&struct_.comments, None);
        Ok(())
    }

    fn process_package_json(&mut self, path: &Path) -> Result<(), Error> {
        if !self.config.mode.nodejs() && !self.config.mode.bundler() {
            bail!(
                "NPM dependencies have been specified in `{}` but \
                 this is only compatible with the `bundler` and `nodejs` targets",
                path.display(),
            );
        }

        let contents =
            fs::read_to_string(path).context(format!("failed to read `{}`", path.display()))?;
        let json: serde_json::Value = serde_json::from_str(&contents)?;
        let object = match json.as_object() {
            Some(s) => s,
            None => bail!(
                "expected `package.json` to have an JSON object in `{}`",
                path.display()
            ),
        };
        let mut iter = object.iter();
        let (key, value) = match iter.next() {
            Some(pair) => pair,
            None => return Ok(()),
        };
        if key != "dependencies" || iter.next().is_some() {
            bail!(
                "NPM manifest found at `{}` can currently only have one key, \
                 `dependencies`, and no other fields",
                path.display()
            );
        }
        let value = match value.as_object() {
            Some(s) => s,
            None => bail!(
                "expected `dependencies` to be a JSON object in `{}`",
                path.display()
            ),
        };

        for (name, value) in value.iter() {
            let value = match value.as_str() {
                Some(s) => s,
                None => bail!(
                    "keys in `dependencies` are expected to be strings in `{}`",
                    path.display()
                ),
            };
            if let Some((prev, _prev_version)) = self.npm_dependencies.get(name) {
                bail!(
                    "dependency on NPM package `{}` specified in two `package.json` files, \
                     which at the time is not allowed:\n  * {}\n  * {}",
                    name,
                    path.display(),
                    prev.display(),
                )
            }

            self.npm_dependencies
                .insert(name.to_string(), (path.to_path_buf(), value.to_string()));
        }

        Ok(())
    }

    fn expose_debug_string(&mut self) {
        if !self.should_write_global("debug_string") {
            return;
        }

        self.global(
            "
           function debugString(val) {
                // primitive types
                const type = typeof val;
                if (type == 'number' || type == 'boolean' || val == null) {
                    return  `${val}`;
                }
                if (type == 'string') {
                    return `\"${val}\"`;
                }
                if (type == 'symbol') {
                    const description = val.description;
                    if (description == null) {
                        return 'Symbol';
                    } else {
                        return `Symbol(${description})`;
                    }
                }
                if (type == 'function') {
                    const name = val.name;
                    if (typeof name == 'string' && name.length > 0) {
                        return `Function(${name})`;
                    } else {
                        return 'Function';
                    }
                }
                // objects
                if (Array.isArray(val)) {
                    const length = val.length;
                    let debug = '[';
                    if (length > 0) {
                        debug += debugString(val[0]);
                    }
                    for(let i = 1; i < length; i++) {
                        debug += ', ' + debugString(val[i]);
                    }
                    debug += ']';
                    return debug;
                }
                // Test for built-in
                const builtInMatches = /\\[object ([^\\]]+)\\]/.exec(toString.call(val));
                let className;
                if (builtInMatches.length > 1) {
                    className = builtInMatches[1];
                } else {
                    // Failed to match the standard '[object ClassName]'
                    return toString.call(val);
                }
                if (className == 'Object') {
                    // we're a user defined class or Object
                    // JSON.stringify avoids problems with cycles, and is generally much
                    // easier than looping through ownProperties of `val`.
                    try {
                        return 'Object(' + JSON.stringify(val) + ')';
                    } catch (_) {
                        return 'Object';
                    }
                }
                // errors
                if (val instanceof Error) {
                    return `${val.name}: ${val.message}\\n${val.stack}`;
                }
                // TODO we could test for more things here, like `Set`s and `Map`s.
                return className;
            }
        ",
        );
    }

    fn export_function_table(&mut self) -> Result<(), Error> {
        if !self.should_write_global("wbg-function-table") {
            return Ok(())
        }
        let id = match self.module.tables.main_function_table()? {
            Some(id) => id,
            None => bail!("no function table found in module"),
        };
        self.module.exports.add("__wbg_function_table", id);
        Ok(())
    }
}

fn generate_identifier(name: &str, used_names: &mut HashMap<String, usize>) -> String {
    let cnt = used_names.entry(name.to_string()).or_insert(0);
    *cnt += 1;
    // We want to mangle `default` at once, so we can support default exports and don't generate
    // invalid glue code like this: `import { default } from './module';`.
    if *cnt == 1 && name != "default" {
        name.to_string()
    } else {
        format!("{}{}", name, cnt)
    }
}

fn format_doc_comments(comments: &str, js_doc_comments: Option<String>) -> String {
    let body: String = comments.lines().map(|c| format!("*{}\n", c)).collect();
    let doc = if let Some(docs) = js_doc_comments {
        docs.lines().map(|l| format!("* {} \n", l)).collect()
    } else {
        String::new()
    };
    format!("/**\n{}{}*/\n", body, doc)
}

fn require_class<'a>(
    exported_classes: &'a mut Option<BTreeMap<String, ExportedClass>>,
    name: &str,
) -> &'a mut ExportedClass {
    exported_classes
        .as_mut()
        .expect("classes already written")
        .entry(name.to_string())
        .or_insert_with(ExportedClass::default)
}

impl ExportedClass {
    fn push(&mut self, docs: &str, function_name: &str, function_prefix: &str, js: &str, ts: &str) {
        self.contents.push_str(docs);
        self.contents.push_str(function_prefix);
        self.contents.push_str(function_name);
        self.contents.push_str(js);
        self.contents.push_str("\n");
        self.typescript.push_str(docs);
        self.typescript.push_str("  ");
        self.typescript.push_str(function_prefix);
        self.typescript.push_str(ts);
        self.typescript.push_str("\n");
    }

    /// Used for adding a field to a class, mainly to ensure that TypeScript
    /// generation is handled specially.
    ///
    /// Note that the `ts` is optional and it's expected to just be the field
    /// type, not the full signature. It's currently only available on getters,
    /// but there currently has to always be at least a getter.
    fn push_field(&mut self, docs: &str, field: &str, js: &str, ts: Option<&str>, getter: bool) {
        self.contents.push_str(docs);
        if getter {
            self.contents.push_str("get ");
        } else {
            self.contents.push_str("set ");
        }
        self.contents.push_str(field);
        self.contents.push_str(js);
        self.contents.push_str("\n");
        let (ty, has_setter) = self
            .typescript_fields
            .entry(field.to_string())
            .or_insert_with(Default::default);
        if let Some(ts) = ts {
            *ty = ts.to_string();
        }
        *has_setter = *has_setter || !getter;
    }
}

#[test]
fn test_generate_identifier() {
    let mut used_names: HashMap<String, usize> = HashMap::new();
    assert_eq!(
        generate_identifier("someVar", &mut used_names),
        "someVar".to_string()
    );
    assert_eq!(
        generate_identifier("someVar", &mut used_names),
        "someVar2".to_string()
    );
    assert_eq!(
        generate_identifier("default", &mut used_names),
        "default1".to_string()
    );
    assert_eq!(
        generate_identifier("default", &mut used_names),
        "default2".to_string()
    );
}
