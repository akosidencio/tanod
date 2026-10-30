import { notFound } from "next/navigation";
import { buildCatalog, catalogUnits } from "@/lib/catalog";
import { beginRender, finishRender } from "@/lib/render";

export const dynamic = "force-dynamic";

type CatalogPageProps = {
  params: Promise<{ slug: string }>;
};

// The opt-in constrained workload for bench/showcase: real CPU work instead of
// a sleep. Off unless TANOD_FIXTURE_CPU_ROUTE=1.
export default async function CatalogPage({ params }: CatalogPageProps) {
  if (process.env.TANOD_FIXTURE_CPU_ROUTE !== "1") {
    notFound();
  }
  const { slug } = await params;
  if (!/^[a-z0-9-]+$/i.test(slug)) {
    notFound();
  }

  const render = beginRender(`/catalog/${slug}`);
  let catalog;
  try {
    catalog = await buildCatalog(slug, catalogUnits());
  } finally {
    finishRender(render);
  }

  return (
    <main
      data-origin-instance={render.instance}
      data-render-id={render.id}
      data-route-kind="cpu-bound-ssr"
    >
      <p className="eyebrow">
        CPU-bound public SSR · {catalog.items} items · checksum {catalog.checksum}
      </p>
      <h1>Catalog: {slug.replaceAll("-", " ")}</h1>
      <p>
        {catalog.facets.map(([category, count]) => `${category} ${count}`).join(" · ")}
      </p>
      <p>Price bands: {catalog.priceBands.join(" / ")}</p>
      <div className="grid">
        {catalog.top.map((item) => (
          <article key={item.id}>
            <h2>{item.name}</h2>
            <p>
              ${item.price.toFixed(2)} · {item.rating.toFixed(1)}★ · {item.category}
            </p>
          </article>
        ))}
      </div>
    </main>
  );
}
