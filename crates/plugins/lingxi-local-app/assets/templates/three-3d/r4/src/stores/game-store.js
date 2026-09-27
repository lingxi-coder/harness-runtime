import { create } from "zustand";

/// App-owned. The phase machine a drawn app needs, in place of the
/// loading/empty/error state list a form-shaped app needs.
///
/// Keep MUTABLE SIMULATION STATE OUT OF HERE. Positions, velocities and
/// particles belong in a ref that the frame loop writes directly: pushing them
/// through a store re-renders React 60 times a second and turns a smooth game
/// into a slideshow. This store holds only what the UI layer renders — the
/// phase, the score, the settings — which changes a few times per session.
export const PHASES = ["menu", "playing", "paused", "over"];

export const useGameStore = create((set) => ({
  phase: "menu",
  score: 0,
  best: 0,

  start: () => set({ phase: "playing", score: 0 }),
  pause: () => set((state) => (state.phase === "playing" ? { phase: "paused" } : state)),
  resume: () => set((state) => (state.phase === "paused" ? { phase: "playing" } : state)),
  addScore: (points) => set((state) => ({ score: state.score + points })),
  gameOver: () =>
    set((state) => ({
      phase: "over",
      best: Math.max(state.best, state.score),
    })),
}));
