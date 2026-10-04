import Image from "next/image";
import Link from "next/link";

export default function HomePage() {
  return (
    <main>
      <section className="hero">
        <div>
          <p className="eyebrow">Deterministic integration fixture</p>
          <h1>A small store with intentionally expensive renders.</h1>
          <p>
            Every route exists to prove one Tanod behavior without relying on
            a production database, payment provider, or hidden framework mock.
          </p>
          <Link className="button" href="/products/atlas-runner">
            Render a product
          </Link>
        </div>
        <Image
          src="/product.svg"
          width={480}
          height={360}
          sizes="(max-width: 700px) 100vw, 480px"
          priority
          alt="Abstract running shoe used by the fixture storefront"
        />
      </section>
      <section className="grid" aria-label="Test scenarios">
        <article>
          <h2>Public SSR</h2>
          <p>Safe microcaching and one-render request coalescing.</p>
        </article>
        <article>
          <h2>Private state</h2>
          <p>Cookies, account pages, carts, and Set-Cookie barriers.</p>
        </article>
        <article>
          <h2>Streaming</h2>
          <p>A fast shell followed by inventory rendered under Suspense.</p>
        </article>
      </section>
    </main>
  );
}
