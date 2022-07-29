// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

// deno-lint-ignore-file

"use strict";

((window) => {
  const {
    ArrayPrototypeIncludes,
    ArrayPrototypeSlice,
    ArrayPrototypePush,
    SafeWeakMap,
  } = window.__bootstrap.primordials;

  let requireDepth = 0;
  let statCache = null;
  let isPreloading = false;

  function updateChildren(parent, child, scan) {
    if (!parent) {
      return;
    }

    const children = parent.children;
    if (children && !(scan && ArrayPrototypeIncludes(children, child))) {
      ArrayPrototypePush(children, child);
    }
  }

  const moduleParentCache = new SafeWeakMap();
  function Module(id = "", parent) {
    this.id = id;
    this.path = path.dirname(id);
    this.exports = {};
    moduleParentCache.set(this, parent);
    updateChildren(parent, this, false);
    this.filename = null;
    this.loaded = false;
    this.children = [];
  }

  const builtinModules = [];
  // TODO(bartlomieju): handle adding native modules
  Module.builtinModules = builtinModules;

  Module._extensions = Object.create(null);
  Module._cache = Object.create(null);
  Module._pathCache = Object.create(null);
  let modulePaths = [];
  Module.globalPaths = modulePaths;

  Module.prototype._findPath = function (request, paths, isMain) {
    throw new Error("not implemented");
  };

  Module.prototype._nodeModulePaths = function (from) {
    // TODO(bartlomieju): this should implemented in Rust as an op
    throw new Error("not implemented");
  };

  Module.prototype._resolveLookupPaths = function (request, parent) {
    throw new Error("not implemented");
  };

  Module.prototype._load = function (request, parent, isMain) {
    throw new Error("not implemented");
  };

  Module.prototype._resolveFilename = function (
    request,
    parent,
    isMain,
    options,
  ) {
    throw new Error("not implemented");
  };

  Module.prototype.load = function (filename) {
    throw new Error("not implemented");
  };

  // Loads a module at the given file path. Returns that module's
  // `exports` property.
  Module.prototype.require = function (id) {
    if (typeof id !== "string") {
      // TODO(bartlomieju): it should use different error type
      // ("ERR_INVALID_ARG_VALUE")
      throw new TypeError("Invalid argument type");
    }

    if (id === "") {
      // TODO(bartlomieju): it should use different error type
      // ("ERR_INVALID_ARG_VALUE")
      throw new TypeError("id must be non empty");
    }
    requireDepth++;
    try {
      return Module._load(id, this, /* isMain */ false);
    } finally {
      requireDepth--;
    }
  };

  Module.prototype._compile = function (content, filename) {
    throw new Error("not implemented");
  };

  Module._extensions[".js"] = function (module, filename) {
    throw new Error("not implemented");
  };

  // Native extension for .json
  Module._extensions[".json"] = function (module, filename) {
    throw new Error("not implemented");
  };

  // Native extension for .node
  Module._extensions[".node"] = function (module, filename) {
    throw new Error("not implemented");
  };

  function createRequireFromPath(filename) {
    throw new Error("not implemented");
  }

  function createRequire(filename) {
    throw new Error("not implemented");
  }

  Module.createRequire = createRequire;

  Module._initPaths = function () {
    const paths = core.opSync("op_require_init_paths");
    modulePaths = paths;
    Module.globalPaths = ArrayPrototypeSlice(modulePaths);
  };

  Module.syncBuiltinESMExports = function syncBuiltinESMExports() {
    throw new Error("not implemented");
  };

  Module.Module = Module;

  window.__bootstrap.require = {
    Module,
  };
})(globalThis);
