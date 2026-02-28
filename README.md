# Project 52F — Dashboard & Bot

## Dashboard (`dashboard.html`)

Single HTML file. Drop into your GitHub Pages repo alongside `index.html`.

It will be live at: `project52f.uk/dashboard`

**Mock mode** — works immediately, simulates live protocol activity every 3 seconds.  
**Live mode** — toggle to LIVE once contracts are deployed. Needs two lines filled in:

```js
// In dashboard.html, update CONFIG:
rpcUrl:       'wss://rpc.qfnetwork.xyz',
tokenEngine:  '0x<deployed_address>',
dampener:     '0x<deployed_address>',
sequencer:    '0x<deployed_address>',
```

Then replace `fetchLiveData()` stub with real polkadot.js contract reads.

---

## Telegram Bot (`bot.js`)

### Setup

```bash
# Install dependencies
npm install

# Get a bot token from @BotFather on Telegram
# Add the bot as admin to your Telegram channel
# Then run:

BOT_TOKEN=1234567890:ABCDEF... \
CHANNEL_ID=@project52f \
node bot.js
```

**Simulation mode** fires synthetic events every 15 seconds so you can see all message formats immediately — no chain required.

### Running on Oracle Cloud

```bash
# Install PM2 for persistent running
npm install -g pm2

# Start and keep alive
pm2 start bot.js --name 52f-bot \
  --env BOT_TOKEN=xxx \
  --env CHANNEL_ID=@project52f

# Auto-restart on reboot
pm2 startup
pm2 save
```

### Wiring to real chain events

When contracts are deployed, replace the stub in `connectToChain()` with:

```js
const { ApiPromise, WsProvider } = require('@polkadot/api');

const provider = new WsProvider(CONFIG.RPC_URL);
const api = await ApiPromise.create({ provider });

// Subscribe to all contract events
api.query.system.events((events) => {
  events.forEach(({ event }) => {
    if (event.section === 'contracts' && event.method === 'ContractEmitted') {
      const [contractAddr, data] = event.data;
      const addr = contractAddr.toString();
      if (addr === CONFIG.TOKEN_ENGINE) decodeAndDispatch('TokenEngine', data);
      if (addr === CONFIG.DAMPENER)     decodeAndDispatch('Dampener',    data);
      if (addr === CONFIG.SEQUENCER)    decodeAndDispatch('Sequencer',   data);
    }
  });
});
```

`decodeAndDispatch` uses the compiled contract ABI (`*.json` output from `cargo contract build`) to decode event data:

```js
const { Abi } = require('@polkadot/api-contract');
const tokenAbi = new Abi(require('./project52f.json'));

function decodeAndDispatch(contractType, rawData) {
  try {
    const decoded = tokenAbi.decodeEvent(rawData);
    handleEvent(contractType, decoded.event.identifier, decoded.args);
  } catch (e) {
    console.error('[DECODE]', e.message);
  }
}
```

### Events & Messages

| Event | Message Type | Throttled? |
|-------|-------------|-----------|
| `CollisionDetected` | 🎯 Big win announcement | No |
| `EpochRolledOver` | 🔄 Rollover update | No |
| `GreatDrain` | 🔥 Dramatic burn event | Deduped by drain_id |
| `PrizePotPulled` | 📤 Epoch yield distributed | No |
| `LiquidityInjected` | 💧 Floor defended | Min 10 QF filter |
| `LiquidityHealthy` | — | Silenced (too noisy) |
| `PhaseTransitioned` | ⚔️ Hardening→Scarcity | Once only |
| `VestingClaimed` | 📋 Founder transparency | Deduped by tranche |
| `WatchdogDrainExecuted` | 👁 Watchdog fired | Deduped by drain_id |
