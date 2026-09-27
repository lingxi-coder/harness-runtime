import { IonButton, IonContent, IonModal, IonTitle } from "@ionic/react";
import { useEffect, useRef } from "react";
import * as THREE from "three";
import { createFrameLoop } from "@/lib/frame-loop";
import { useGameStore } from "@/src/stores/game-store";

export function GameScreen() {
  const canvasRef = useRef(null);
  const sceneRef = useRef(null);
  const rendererRef = useRef(null);
  const cameraRef = useRef(null);
  const cubeRef = useRef(null);

  const phase = useGameStore((state) => state.phase);
  const score = useGameStore((state) => state.score);
  const best = useGameStore((state) => state.best);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;

    const scene = new THREE.Scene();
    scene.background = new THREE.Color("#07111f");

    const camera = new THREE.PerspectiveCamera(55, 1, 0.1, 100);
    camera.position.set(0, 0.8, 4.8);

    const renderer = new THREE.WebGLRenderer({
      canvas,
      antialias: true,
      alpha: false,
      powerPreference: "high-performance",
    });
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 3));

    const ambient = new THREE.AmbientLight("#9fc6ff", 1.1);
    const key = new THREE.DirectionalLight("#ffffff", 1.6);
    key.position.set(4, 6, 6);
    const rim = new THREE.DirectionalLight("#56d4ff", 0.8);
    rim.position.set(-5, -2, -3);
    scene.add(ambient, key, rim);

    const cube = new THREE.Mesh(
      new THREE.BoxGeometry(1.35, 1.35, 1.35),
      new THREE.MeshStandardMaterial({
        color: "#6ee7ff",
        metalness: 0.2,
        roughness: 0.28,
      }),
    );
    scene.add(cube);

    sceneRef.current = scene;
    rendererRef.current = renderer;
    cameraRef.current = camera;
    cubeRef.current = cube;

    const loop = createFrameLoop(canvas, {
      onResize: ({ width, height }) => {
        camera.aspect = width / Math.max(1, height);
        camera.updateProjectionMatrix();
        renderer.setSize(width, height, false);
      },
      onFrame: ({ dt }) => {
        if (useGameStore.getState().phase === "playing") {
          cube.rotation.x += dt * 0.9;
          cube.rotation.y += dt * 1.2;
        }
        renderer.render(scene, camera);
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
    loop.start();

    return () => {
      loop.stop();
      canvas.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("keydown", onKeyDown);
      cube.geometry.dispose();
      cube.material.dispose();
      renderer.dispose();
      scene.clear();
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
            {phase === "menu" ? "Three.js 预览" : phase === "paused" ? "已暂停" : "本局结束"}
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
