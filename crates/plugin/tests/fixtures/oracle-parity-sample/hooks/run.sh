#!/bin/sh
# Sample fixture hook for the P0a.8 oracle-parity-sample plugin tree. Mirrors
# plugin/tests/fixtures/brand-normalize-sample/hooks/run.sh: the plugin
# manager substitutes the manifest's ${LINGXI_PLUGIN_ROOT} / ${LINGXI_PROJECT_DIR}
# tokens before spawning this command hook, and also injects them into this
# process's own environment under the same names. Deliberately unquoted below
# (a fixture-authoring choice) so this line stays outside any string literal.
exec $LINGXI_PLUGIN_ROOT/bin/sample-tool --project $LINGXI_PROJECT_DIR
