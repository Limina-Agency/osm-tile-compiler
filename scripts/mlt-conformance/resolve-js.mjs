// @maplibre/mlt's ES modules import each other without a file extension,
// as bundlers allow and Node does not: this resolve hook tries `.js`.
import { registerHooks } from "node:module";

registerHooks({
  resolve(specifier, context, next) {
    try {
      return next(specifier, context);
    } catch (error) {
      if (error.code !== "ERR_MODULE_NOT_FOUND" || !/^\.\.?\//.test(specifier)) throw error;
      return next(`${specifier}.js`, context);
    }
  },
});
