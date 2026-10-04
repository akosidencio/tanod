import type { Metadata } from "next";
import Link from "next/link";
import type { ReactNode } from "react";
import "./globals.css";

export const metadata: Metadata = {
  title: "Tanod Test Store",
  description: "A deterministic Next.js origin for Tanod integration tests",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en">
      <body>
        <header className="site-header">
          <Link href="/" className="brand">
            Tanod Test Store
          </Link>
          <nav aria-label="Primary navigation">
            <Link href="/products/atlas-runner">Product</Link>
            <Link href="/search?q=runner">Search</Link>
            <Link href="/flash-sale">Flash sale</Link>
            <Link href="/account">Account</Link>
            <Link href="/cart">Cart</Link>
            <Link href="/preview">Preview</Link>
          </nav>
        </header>
        {children}
      </body>
    </html>
  );
}
