import Phaser from "phaser";
import { createFrameLoop } from "@/lib/frame-loop";

/**
 * Host-owned Phaser lifecycle. The editable screen supplies scene callbacks;
 * this adapter owns the canvas, scheduler, resize path and teardown.
 */
export function createPhaserRuntime(canvas, callbacks = {}) {
  const { onCreate, onFrame, onResize } = callbacks;
  const initialRect = canvas.getBoundingClientRect();
  const initialWidth = Math.max(1, Math.round(initialRect.width || canvas.width || 1));
  const initialHeight = Math.max(1, Math.round(initialRect.height || canvas.height || 1));
  let phaserTime = 0;
  let internalLoopStopped = false;
  let disposed = false;
  let destroyed = false;

  const game = new Phaser.Game({
    type: Phaser.CANVAS,
    canvas,
    parent: null,
    width: initialWidth,
    height: initialHeight,
    backgroundColor: "#07111f",
    scale: { mode: Phaser.Scale.NONE },
    scene: {
      create() {
        onCreate?.(this);
      },
    },
  });

  const destroyGame = () => {
    if (destroyed || !game.isBooted) return;
    destroyed = true;
    game.destroy(false);
    game.step(performance.now(), 0);
  };

  // Phaser starts its own TimeStep after boot. Stop it after READY so the
  // host createFrameLoop remains the sole scheduler for this surface.
  const stopPhaserLoop = () => {
    queueMicrotask(() => {
      if (destroyed) return;
      if (game.isRunning) game.loop.stop();
      internalLoopStopped = true;
      if (disposed) destroyGame();
    });
  };
  game.events.once("ready", stopPhaserLoop);
  if (game.isBooted && game.isRunning) {
    game.loop.stop();
    internalLoopStopped = true;
  }

  const loop = createFrameLoop(canvas, {
    onResize: (size) => {
      onResize?.(size);
      if (game.isBooted && game.renderer) {
        // Update Phaser's logical camera size without letting its NONE-scale
        // resize path overwrite the host-owned responsive CSS dimensions.
        game.scale.setGameSize(size.width, size.height);
        // ScaleManager writes a logical canvas size; restore the helper's
        // high-DPI backing store before the next render.
        canvas.width = size.bufferWidth;
        canvas.height = size.bufferHeight;
        game.renderer.resize(size.bufferWidth, size.bufferHeight);
      }
    },
    onFrame: (frame) => {
      onFrame?.(frame);
      if (internalLoopStopped && game.isBooted) {
        phaserTime += frame.dt * 1000;
        game.step(phaserTime, frame.dt * 1000);
      }
    },
  });

  return {
    get size() {
      return loop.size;
    },
    start() {
      if (disposed) return;
      loop.start();
    },
    stop() {
      if (disposed) return;
      disposed = true;
      loop.stop();
      destroyGame();
    },
  };
}
