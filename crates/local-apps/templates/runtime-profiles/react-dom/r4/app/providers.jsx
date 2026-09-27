import { useRef } from "react";
import { IonApp, setupIonicReact } from "@ionic/react";
import { IonReactHashRouter } from "@ionic/react-router";
import { ErrorBoundary } from "@/app/error-boundary";
import { LingXiBridgeProvider, useLingXi } from "@/lib/lingxi-provider";

/// Ionic reads `mode` when a component is CONSTRUCTED, not when it renders, so
/// `setupIonicReact` has to run before the first `ion-*` element exists and the
/// value it is given has to already be right.
///
/// That is why this waits for `bridgeResolved`. `deviceContext` arrives from the
/// native host on `window.lingxi.v2`, which is not guaranteed to exist at module
/// evaluation time; configuring at import would read the fallback context, pick
/// `md`, and leave every iOS app wearing Material chrome for the whole session
/// with nothing failing.
///
/// Ionic would otherwise sniff the user agent. The host is the better authority:
/// it knows which client embeds this WebView.
function IonicHost({ children }) {
  const { adapter, bridgeResolved } = useLingXi();
  const configured = useRef(false);

  if (!bridgeResolved) return null;

  if (!configured.current) {
    setupIonicReact({ mode: adapter.ionicMode });
    configured.current = true;
  }

  return (
    <IonApp>
      <IonReactHashRouter>{children}</IonReactHashRouter>
    </IonApp>
  );
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
