/// Profile-managed. The three things every drawn surface gets wrong, solved once.
///
/// 1. BACKING-STORE SIZE. A canvas has two sizes: the CSS box and the drawing
///    buffer. Setting only the CSS size renders at 1x and looks soft on every
///    phone; setting the buffer once at mount breaks the moment the window
///    changes, which on iPad happens constantly (rotation, Split View, Stage
///    Manager). This resizes the buffer from a ResizeObserver and reports the
///    CSS size to the caller, so game logic can stay in CSS pixels.
///
/// 2. SUSPEND. `requestAnimationFrame` stops while the app is backgrounded, and
///    the first frame after resuming carries the entire elapsed wall-clock time.
///    An unclamped `dt` teleports everything through walls. Frames are clamped
///    and the clock is reset on resume.
///
/// 3. TEARDOWN. A loop that outlives its component keeps drawing into a
///    detached canvas and pins the whole scene in memory. `stop()` is
///    idempotent and cancels the observer, the listener and the frame.
///
/// Usage:
///   const loop = createFrameLoop(canvas, {
///     onResize: ({ width, height, dpr }) => { … },
///     onFrame: ({ dt, elapsed, width, height }) => { … },
///   });
///   loop.start();
///   return () => loop.stop();   // from a useEffect cleanup

const MAX_FRAME_SECONDS = 1 / 15;

export function createFrameLoop(canvas, options = {}) {
  const { onFrame, onResize, maxDevicePixelRatio = 3 } = options;

  let frame = 0;
  let running = false;
  let lastTimestamp = 0;
  let elapsed = 0;
  let width = 0;
  let height = 0;
  let observer = null;

  const applySize = () => {
    if (!canvas) return;
    const rect = canvas.getBoundingClientRect();
    const nextWidth = Math.max(1, Math.round(rect.width));
    const nextHeight = Math.max(1, Math.round(rect.height));
    const dpr = Math.min(window.devicePixelRatio || 1, maxDevicePixelRatio);
    const bufferWidth = Math.round(nextWidth * dpr);
    const bufferHeight = Math.round(nextHeight * dpr);

    if (canvas.width !== bufferWidth || canvas.height !== bufferHeight) {
      canvas.width = bufferWidth;
      canvas.height = bufferHeight;
    }

    width = nextWidth;
    height = nextHeight;
    onResize?.({ width, height, dpr, bufferWidth, bufferHeight });
  };

  const tick = (timestamp) => {
    if (!running) return;
    const seconds = lastTimestamp ? (timestamp - lastTimestamp) / 1000 : 0;
    lastTimestamp = timestamp;
    const dt = Math.min(Math.max(seconds, 0), MAX_FRAME_SECONDS);
    elapsed += dt;
    // Queue the NEXT frame BEFORE running app code. A single throw out of
    // `onFrame` would otherwise skip this line and end the loop for good:
    // `running` stays true, so `start()` returns at its own guard and nothing
    // can revive the surface. The app then shows a still picture, and the
    // build's motion check fails on "the two captures are identical" — sending
    // the agent after a simulation bug that is really one uncaught throw.
    frame = window.requestAnimationFrame(tick);
    onFrame?.({ dt, elapsed, width, height });
  };

  // A resumed tab reports a timestamp far in the future relative to the last
  // one. Dropping the accumulated gap is the point: the player did not see
  // those seconds, so the simulation must not run them.
  const handleVisibility = () => {
    if (document.visibilityState === "visible") {
      lastTimestamp = 0;
      applySize();
    }
  };

  return {
    get size() {
      return { width, height };
    },
    get elapsed() {
      return elapsed;
    },
    start() {
      if (running) return;
      running = true;
      lastTimestamp = 0;

      applySize();
      if (typeof ResizeObserver !== "undefined" && canvas) {
        observer = new ResizeObserver(applySize);
        observer.observe(canvas);
      }
      // A display/DPR change need not change the CSS box observed above.
      window.addEventListener("resize", applySize);
      document.addEventListener("visibilitychange", handleVisibility);

      frame = window.requestAnimationFrame(tick);
    },
    stop() {
      if (!running) return;
      running = false;
      window.cancelAnimationFrame(frame);
      frame = 0;
      observer?.disconnect();
      observer = null;
      window.removeEventListener("resize", applySize);
      document.removeEventListener("visibilitychange", handleVisibility);
    },
  };
}
