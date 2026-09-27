import { IonRouterOutlet } from "@ionic/react";
import { Navigate, Route, Routes } from "react-router-dom";
import { DetailScreen } from "@/app/screens/detail-screen";
import { HomeScreen } from "@/app/screens/home-screen";

/// `IonRouterOutlet` is what turns route changes into native page transitions
/// and enables the platform back gesture, so routes belong INSIDE it. It reads
/// its `<Routes>` children directly and treats a `<Navigate>` element as a
/// redirect.
///
/// Every routed component must render an `<IonPage>` as its root, otherwise the
/// outlet has nothing to animate and the screen appears without a transition.
export function App() {
  return (
    <IonRouterOutlet>
      <Routes>
        <Route path="/" element={<HomeScreen />} />
        <Route path="/detail" element={<DetailScreen />} />
        <Route path="*" element={<Navigate replace to="/" />} />
      </Routes>
    </IonRouterOutlet>
  );
}
