// Fixture workflow script for the P0a.8 oracle/LingXi parity test.
// Reads the plugin's materialized install root via the injected env var —
// the same substitution surface as the manifest's ${LINGXI_PLUGIN_ROOT}
// hook/mcpServers tokens (see plugin/src/manifest.rs's PLUGIN_ROOT_ENV doc).
module.exports.meta = { name: "build" };
module.exports.run = async () => {
  return process.env.LINGXI_PLUGIN_ROOT;
};
