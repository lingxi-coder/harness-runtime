import { IonButton, IonContent, IonModal, IonTitle } from "@ionic/react";
import { useEffect, useRef } from "react";
import { createBabylonRuntime } from "@/lib/babylon-runtime";
import { useGameStore } from "@/src/stores/game-store";

export function GameScreen() {
  const canvasRef = useRef(null);

  const phase = useGameStore((state) => state.phase);
  const score = useGameStore((state) => state.score);
  const best = useGameStore((state) => state.best);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;

    const runtime = createBabylonRuntime(canvas, {
      onFrame: ({ dt, cube }) => {
        if (useGameStore.getState().phase === "playing") {
          cube.rotation.x += dt * 0.9;
          cube.rotation.y += dt * 1.2;
        }
      },
    });

    const pulse = () => {
      const state = useGameStore.getState();
      state.addScore(1);
    };
    const onPointerDown = () => pulse();
    const onKeyDown = (event) => {
      if (event.key === " " || event.key === "Enter") {
        event.preventDefault();
        pulse();
      }
    };

    canvas.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("keydown", onKeyDown);
    runtime.start();

    return () => {
      runtime.stop();
      canvas.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("keydown", onKeyDown);
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
          color: "var(--ion-color-light, #fff)",
        }}
      >
        <span>旋转分数 {score}</span>
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
        initialBreakpoint={0.42}
        breakpoints={[0.42]}
      >
        <IonContent className="ion-padding ion-text-center">
          <IonTitle>
            {phase === "menu" ? "Babylon.js 预览" : phase === "paused" ? "已暂停" : "本局结束"}
          </IonTitle>
          <p>点击或按空格让立方体继续旋转。最高分 {best}</p>
          <IonButton expand="block" onClick={phase === "paused" ? resume : start}>
            {phase === "paused" ? "继续" : "开始"}
          </IonButton>
        </IonContent>
      </IonModal>
    </>
  );
}
