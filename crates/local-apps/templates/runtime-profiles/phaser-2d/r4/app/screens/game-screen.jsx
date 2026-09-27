import { IonButton, IonContent, IonModal, IonTitle } from "@ionic/react";
import { useEffect, useRef } from "react";
import { createPhaserRuntime } from "@/lib/phaser-runtime";
import { useGameStore } from "@/src/stores/game-store";
import { useLingXi } from "@/lib/lingxi-provider";

/// The default editable entry point for a drawn app. Replace the simulation
/// freely; keep the shape:
///
///   - ONE `<canvas className="lingxi-canvas-surface">` filling the viewport
///   - the host-managed Phaser runtime, started in an effect and stopped in
///     its cleanup
///   - simulation state in a `useRef`, NOT in the store — see game-store.js
///   - overlays (menu, pause, game over) as Ionic components ON TOP of the
///     canvas, so they carry the platform look and get focus handling for free
export function GameScreen() {
  const canvasRef = useRef(null);
  // `placed` is an explicit flag, not a 0/0 sentinel: (0, 0) is a legitimate
  // position, so testing coordinates cannot tell "not initialized yet" from
  // "sitting in the corner".
  const worldRef = useRef({ placed: false, x: 0, y: 0, targetX: 0, targetY: 0, pointer: null });
  const { adapter } = useLingXi();

  const phase = useGameStore((state) => state.phase);
  const score = useGameStore((state) => state.score);
  const best = useGameStore((state) => state.best);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    const world = worldRef.current;
    let target = null;
    let player = null;

    const place = (width, height) => {
      world.targetX = 40 + Math.random() * Math.max(1, width - 80);
      world.targetY = 40 + Math.random() * Math.max(1, height - 80);
    };

    const runtime = createPhaserRuntime(canvas, {
      onCreate: (scene) => {
        target = scene.add.circle(0, 0, 12, 0xffb000);
        player = scene.add.circle(0, 0, 16, 0x3dd6a0);
      },
      onResize: ({ width, height }) => {
        if (!world.placed) {
          world.placed = true;
          world.x = width / 2;
          world.y = height / 2;
          place(width, height);
        } else {
          // A rotation or an iPad multitasking drag SHRINKS the box. Everything
          // positioned for the old size has to be brought back inside it — a
          // target left outside can never be reached, because the pointer cannot
          // leave the canvas, and the app becomes permanently unscoreable.
          world.x = Math.min(world.x, width);
          world.y = Math.min(world.y, height);
          // The pointer has to be clamped with them. `onFrame` eases `world.x`
          // toward `world.pointer.x` every tick, so a stale pointer left outside
          // the new box drags the ball straight back out on the very next frame —
          // and no pointer event fires during a rotation to refresh it.
          if (world.pointer) {
            world.pointer.x = Math.min(world.pointer.x, width);
            world.pointer.y = Math.min(world.pointer.y, height);
          }
          if (world.targetX > width - 40 || world.targetY > height - 40) {
            place(width, height);
          }
        }
      },
      onFrame: ({ dt, width, height }) => {
        const { phase: current } = useGameStore.getState();

        if (current === "playing") {
          const goalX = world.pointer?.x ?? world.x;
          const goalY = world.pointer?.y ?? world.y;
          // Frame-rate independent easing: the exponent makes the result the
          // same at 60 Hz and at 120 Hz.
          const ease = 1 - Math.pow(0.0001, dt);
          world.x += (goalX - world.x) * ease;
          world.y += (goalY - world.y) * ease;

          const dx = world.x - world.targetX;
          const dy = world.y - world.targetY;
          if (Math.hypot(dx, dy) < 28) {
            useGameStore.getState().addScore(1);
            place(width, height);
          }
        }

        target?.setPosition(world.targetX, world.targetY);
        player?.setPosition(world.x, world.y);
      },
    });

    // Pointer Events cover touch, trackpad and Pencil in one path; `inputMode`
    // is not always "touch" (iPad with a trackpad reports a fine pointer).
    const toLocal = (event) => {
      const rect = canvas.getBoundingClientRect();
      return { x: event.clientX - rect.left, y: event.clientY - rect.top };
    };
    const onPointerMove = (event) => {
      world.pointer = toLocal(event);
    };
    const onPointerLeave = () => {
      world.pointer = null;
    };

    canvas.addEventListener("pointerdown", onPointerMove);
    canvas.addEventListener("pointermove", onPointerMove);
    canvas.addEventListener("pointerup", onPointerLeave);
    canvas.addEventListener("pointercancel", onPointerLeave);

    runtime.start();

    return () => {
      runtime.stop();
      canvas.removeEventListener("pointerdown", onPointerMove);
      canvas.removeEventListener("pointermove", onPointerMove);
      canvas.removeEventListener("pointerup", onPointerLeave);
      canvas.removeEventListener("pointercancel", onPointerLeave);
    };
  }, []);

  const { start, pause, resume } = useGameStore.getState();

  return (
    <>
      <canvas ref={canvasRef} className="lingxi-canvas-surface" />

      <div
        style={{
          position: "fixed",
          top: "calc(var(--safe-area-top, 0px) + 12px)",
          insetInline: "12px",
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          pointerEvents: "none",
          font: "600 1rem var(--platform-font, system-ui, sans-serif)",
          color: "var(--ion-text-color, #000)",
        }}
      >
        <span>得分 {score}</span>
        <IonButton
          size="small"
          fill="clear"
          style={{ pointerEvents: "auto" }}
          onClick={pause}
          disabled={phase !== "playing"}
        >
          暂停
        </IonButton>
      </div>

      <IonModal
        isOpen={phase !== "playing"}
        backdropDismiss={false}
        // A pause sheet is not a full page; the platform sheet presentation is
        // what makes it read as native on both iOS and Android.
        initialBreakpoint={0.4}
        breakpoints={[0.4]}
      >
        <IonContent className="ion-padding ion-text-center">
          <IonTitle>
            {phase === "menu" ? "准备开始" : phase === "paused" ? "已暂停" : "本局结束"}
          </IonTitle>
          <p>最好成绩 {best}　·　{adapter.ionicMode === "ios" ? "iOS" : "Material"}</p>
          <IonButton expand="block" onClick={phase === "paused" ? resume : start}>
            {phase === "paused" ? "继续" : "开始"}
          </IonButton>
        </IonContent>
      </IonModal>
    </>
  );
}
