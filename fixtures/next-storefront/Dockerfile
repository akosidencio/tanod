FROM node:22-alpine AS dependencies
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci

FROM node:22-alpine AS builder
WORKDIR /app
ARG TANOD_BUILD_ID=next-fixture-v1
ARG NEXT_SERVER_ACTIONS_ENCRYPTION_KEY=MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=
ENV TANOD_BUILD_ID=$TANOD_BUILD_ID
ENV NEXT_SERVER_ACTIONS_ENCRYPTION_KEY=$NEXT_SERVER_ACTIONS_ENCRYPTION_KEY
COPY --from=dependencies /app/node_modules ./node_modules
COPY . .
RUN npm run build
RUN node -e "require('node:fs').writeFileSync('tanod.deployment.json', JSON.stringify({version:1,build_id:process.env.TANOD_BUILD_ID}, null, 2) + '\\n')"

FROM node:22-alpine AS runtime
WORKDIR /app

ENV NODE_ENV=production
ENV HOSTNAME=0.0.0.0
ENV PORT=3000

RUN addgroup --system --gid 1001 nodejs \
    && adduser --system --uid 1001 nextjs

COPY --from=builder --chown=nextjs:nodejs /app/.next/standalone ./
COPY --from=builder --chown=nextjs:nodejs /app/.next/static ./.next/static
COPY --from=builder --chown=nextjs:nodejs /app/public ./public
COPY --from=builder --chown=nextjs:nodejs /app/tanod.deployment.json ./tanod.deployment.json

USER nextjs
EXPOSE 3000
CMD ["node", "server.js"]
