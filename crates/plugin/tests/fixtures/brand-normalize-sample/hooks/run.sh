#!/bin/sh
# Sample fixture hook for the P0a.7 brand-normalize-sample plugin tree.
#
# The plugin manager substitutes the manifest's ${LINGXI_PLUGIN_ROOT} /
# ${LINGXI_PLUGIN_DATA} / ${LINGXI_PROJECT_DIR} tokens before spawning a
# Command hook (see plugin/src/manager.rs); it also injects them into this
# process's own environment under the same names, so a hook script can read
# them directly. Deliberately unquoted below (a fixture-authoring choice, not
# a shell-safety recommendation) so this line stays outside any string
# literal on a non-comment line.
exec $LINGXI_PLUGIN_ROOT/bin/sample-tool --project $LINGXI_PROJECT_DIR
