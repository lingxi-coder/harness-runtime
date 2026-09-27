export const metadata = {
  title: 'hello-next-spike',
  description: 'LingXi local-apps phase-0 app-runtime spike template',
};

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
