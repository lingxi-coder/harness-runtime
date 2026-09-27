import { useRef } from "react";
import { IonApp, setupIonicReact } from "@ionic/react";
import { ErrorBoundary } from "@/app/error-boundary";
import { LingXiBridgeProvider, useLingXi } from "@/lib/lingxi-provider";

/// Same contract as the DOM scaffold — see the comment there for why this waits
/// for `bridgeResolved` before calling `setupIonicReact`.
///
/// The difference is that there is NO router. A drawn app is one surface plus
/// overlays; a router outlet would add a page stack with nothing to push onto
/// it, and its transitions would fight the frame loop for the compositor.
function IonicHost({ children }) {
  const { adapter, bridgeResolved } = useLingXi();
  const configured = useRef(false);

  if (!bridgeResolved) return null;

  if (!configured.current) {
    setupIonicReact({ mode: adapter.ionicMode });
    configured.current = true;
  }

  return <IonApp>{children}</IonApp>;
}

export function AppProviders({ children }) {
  return (
    <LingXiBridgeProvider>
      <ErrorBoundary>
        <IonicHost>{children}</IonicHost>
      </ErrorBoundary>
    </LingXiBridgeProvider>
  );
}
