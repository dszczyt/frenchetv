// Feasibility gate: can wasm_comm_module instantiate under Node?
const fs = require('fs');
const path = require('path');

// --- minimal browser shims ---
globalThis.self = globalThis;
globalThis.window = globalThis;
globalThis.navigator = { userAgent: 'node', hardwareConcurrency: 4 };
globalThis.document = {
  createElement: () => ({ set src(_) {}, set onload(_) {}, set onerror(_) {} }),
  head: { append: () => {} },
};
globalThis.location = { href: 'file://' + __dirname + '/' , origin: 'https://www.bouyguestelecom.fr' };
if (!globalThis.crypto) globalThis.crypto = require('crypto').webcrypto;
// XHR shim (sync+async) backed by node — emscripten uses it for the pfs handshake
try { globalThis.XMLHttpRequest = require('xmlhttprequest').XMLHttpRequest; }
catch { console.log('(no xmlhttprequest pkg yet)'); }

const glue = fs.readFileSync(path.join(__dirname, 'wasm_comm_module.js'), 'utf8');
// Emscripten MODULARIZE: define wasm_comm_module in this scope
const factory = new Function('module', 'exports', 'require', '__dirname', '__filename',
  glue + '\n;return (typeof wasm_comm_module!=="undefined")?wasm_comm_module:(module.exports);');
const mod = {};
const wasm_comm_module = factory(mod, mod, require, __dirname, path.join(__dirname,'wasm_comm_module.js'));
console.log('factory type:', typeof wasm_comm_module);

const Module = {
  locateFile: (p) => path.join(__dirname, p),
  print: (...a)=>console.log('[wasm]',...a),
  printErr: (...a)=>console.log('[wasm-err]',...a),
  onAbort: (w)=>console.log('[ABORT]', w),
};
console.log('instantiating…');
Promise.resolve(wasm_comm_module(Module)).then(m=>{
  console.log('LOADED ✓ exports sample:', Object.keys(m).filter(k=>/Token|Authenticate|Kaltura|Pfs|sync/i.test(k)).slice(0,20));
}).catch(e=>console.log('LOAD FAILED:', e && (e.stack||e.message||e)));
