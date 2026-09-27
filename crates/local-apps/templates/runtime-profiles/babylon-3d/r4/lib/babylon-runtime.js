import {
  ArcRotateCamera,
  Color3,
  Color4,
  Engine,
  HavokPlugin,
  HemisphericLight,
  MeshBuilder,
  PhysicsAggregate,
  PhysicsShapeType,
  Scene,
  StandardMaterial,
  Vector3,
} from "@babylonjs/core";
import HavokPhysics from "@babylonjs/havok";
import "@babylonjs/loaders";
import { createFrameLoop } from "@/lib/frame-loop";

/**
 * Host-owned Babylon lifecycle. The editable screen supplies business
 * callbacks; this adapter owns the engine, scene, physics, frame loop and
 * teardown.
 */
export function createBabylonRuntime(canvas, callbacks = {}) {
  const { onCreate, onFrame, onResize } = callbacks;
  const engine = new Engine(
    canvas,
    true,
    { preserveDrawingBuffer: true, stencil: true },
    true,
  );
  const scene = new Scene(engine);
  scene.clearColor = new Color4(0.027, 0.067, 0.122, 1);

  const camera = new ArcRotateCamera(
    "camera",
    -Math.PI / 2,
    Math.PI / 2.5,
    4.8,
    Vector3.Zero(),
    scene,
  );
  camera.attachControl(canvas, true);

  const light = new HemisphericLight("key", new Vector3(0, 1, 0), scene);
  light.intensity = 1.2;

  const cube = MeshBuilder.CreateBox("cube", { size: 1.35 }, scene);
  cube.position.y = 0.8;
  const cubeMaterial = new StandardMaterial("cube-material", scene);
  cubeMaterial.diffuseColor = new Color3(0.43, 0.91, 1);
  cube.material = cubeMaterial;

  let disposed = false;
  let physicsAggregate = null;
  void HavokPhysics()
    .then((havok) => {
      if (disposed) return;
      const physicsPlugin = new HavokPlugin(true, havok);
      if (scene.enablePhysics(new Vector3(0, -9.81, 0), physicsPlugin)) {
        physicsAggregate = new PhysicsAggregate(
          cube,
          PhysicsShapeType.BOX,
          { mass: 0 },
          scene,
        );
      }
    })
    .catch((error) => {
      if (!disposed) console.warn("Havok physics unavailable", error);
    });

  onCreate?.({ scene, camera, light, cube });

  const loop = createFrameLoop(canvas, {
    onResize: (size) => {
      onResize?.(size);
      // createFrameLoop owns the DPR-aware backing store; Babylon re-reads it
      // here without starting a second render scheduler.
      engine.resize();
    },
    onFrame: (frame) => {
      onFrame?.({ ...frame, scene, camera, cube });
      scene.render();
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
      physicsAggregate?.dispose();
      scene.dispose();
      engine.dispose();
    },
  };
}
