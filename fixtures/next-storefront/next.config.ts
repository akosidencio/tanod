import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  output: "standalone",
  poweredByHeader: false,
  deploymentId: process.env.TANOD_BUILD_ID ?? "next-fixture-v1",
  generateBuildId: async () => process.env.TANOD_BUILD_ID ?? "next-fixture-v1",
  cacheHandler: require.resolve("./cache-handler.cjs"),
  cacheMaxMemorySize: 0,
  async headers() {
    return [
      {
        source: "/products/:slug",
        headers: [
          {
            key: "X-Tanod-Cache-Tags",
            value: "products",
          },
        ],
      },
    ];
  },
  turbopack: {
    root: process.cwd(),
  },
};

export default nextConfig;
