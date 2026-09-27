import { create } from "zustand";

export const useAppStore = create((set) => ({
  completedActions: 0,
  completeAction: () =>
    set((state) => ({ completedActions: state.completedActions + 1 })),
}));
