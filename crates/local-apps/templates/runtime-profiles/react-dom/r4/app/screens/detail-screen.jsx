import {
  IonBackButton,
  IonButtons,
  IonContent,
  IonHeader,
  IonPage,
  IonTitle,
  IonToolbar,
} from "@ionic/react";

/// A second screen exists in the scaffold on purpose: it is what makes the
/// native transition and the platform back gesture observable. `defaultHref` is
/// the fallback the back button uses when this screen was opened directly
/// rather than pushed onto the stack.
export function DetailScreen() {
  return (
    <IonPage>
      <IonHeader>
        <IonToolbar>
          <IonButtons slot="start">
            <IonBackButton defaultHref="/" />
          </IonButtons>
          <IonTitle>详情</IonTitle>
        </IonToolbar>
      </IonHeader>

      <IonContent className="ion-padding">
        <p>返回按钮与侧滑手势由 IonRouterOutlet 提供，不需要自行实现。</p>
      </IonContent>
    </IonPage>
  );
}
