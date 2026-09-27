import { GameScreen } from "@/app/screens/game-screen";

/// One surface, no routes. Menus, pause and game-over are overlays on top of the
/// canvas rather than separate pages — see `app/screens/game-screen.jsx`.
export function App() {
  return <GameScreen />;
}
