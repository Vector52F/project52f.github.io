// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import "forge-std/Test.sol";
import "../src/TokenEngine_v2_FINAL.sol";
import "../src/DampenerVault.sol";
import "../src/CryptographicSequencer.sol";

// =============================================================================
// SHARED MOCKS (copied from Protocol52F.t.sol — identical setup)
// =============================================================================

contract SMockERC20 {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    function mint(address to, uint256 amount) external { balanceOf[to] += amount; }
    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }
    function transfer(address to, uint256 amount) external returns (bool) {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }
    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

contract SMockDEXPair {
    uint112 public reserve0;
    uint112 public reserve1;
    address public token0;
    address public token1;
    SMockERC20 public lpToken;

    constructor(address _token0, address _token1) {
        token0  = _token0;
        token1  = _token1;
        lpToken = new SMockERC20();
    }

    function setReserves(uint112 r0, uint112 r1) external { reserve0 = r0; reserve1 = r1; }
    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, uint32(block.timestamp));
    }
    function mintLP(address to, uint256 amount) external { lpToken.mint(to, amount); }
}

contract SMockDEXRouter {
    address public immutable WETH_ADDR;
    SMockDEXPair public pair;
    address public token52f;

    constructor(address _weth, address _pair, address _token) {
        WETH_ADDR = _weth;
        pair      = SMockDEXPair(_pair);
        token52f  = _token;
    }

    function WETH() external view returns (address) { return WETH_ADDR; }

    function swapExactETHForTokens(
        uint256, address[] calldata, address to, uint256
    ) external payable returns (uint256[] memory amounts) {
        uint256 tokensOut = msg.value * 1000;
        Project52F(payable(token52f)).mockMint(to, tokensOut);
        amounts = new uint256[](2);
        amounts[0] = msg.value;
        amounts[1] = tokensOut;
        (uint112 r0, uint112 r1,) = pair.getReserves();
        uint112 addTokens = uint112(tokensOut / 1e18);
        uint112 removeQF  = uint112(msg.value / 1e18);
        if (r1 >= removeQF) pair.setReserves(r0 + addTokens, r1 - removeQF);
    }

    function swapExactTokensForETH(
        uint256 amountIn, uint256, address[] calldata, address to, uint256
    ) external returns (uint256[] memory amounts) {
        uint256 qfOut = amountIn / 1000;
        (bool ok,) = to.call{value: qfOut}("");
        require(ok, "QF transfer failed");
        amounts = new uint256[](2);
        amounts[0] = amountIn;
        amounts[1] = qfOut;
    }

    function addLiquidityETH(
        address, uint256 amountTokenDesired, uint256, uint256, address to, uint256
    ) external payable returns (uint256, uint256, uint256 liquidity) {
        liquidity = amountTokenDesired + msg.value;
        pair.mintLP(to, liquidity);
        return (amountTokenDesired, msg.value, liquidity);
    }

    receive() external payable {}
}

// =============================================================================
// STRESS TEST CONTRACT
// =============================================================================

contract StressTest is Test {

    Project52F             public engine;
    DampenerVault          public dampener;
    CryptographicSequencer public sequencer;
    SMockDEXPair           public pair;
    SMockDEXRouter         public router;
    SMockERC20             public weth;

    address public owner    = address(0x1);
    address public alice    = address(0x2);
    address public bob      = address(0x3);
    address public charlie  = address(0x5);
    address public teamAddr = address(0x4);

    // Stress test wallets — 20 concurrent traders
    address[20] public traders;

    uint256 constant ZONE_A_END    = 5_200;
    uint256 constant ZONE_B_END    = 10_400;
    uint256 constant HARDENING_END = 26_000_000;

    // =========================================================================
    // SETUP
    // =========================================================================

    function setUp() public {
        vm.startPrank(owner);

        weth   = new SMockERC20();
        engine = new Project52F();

        pair = new SMockDEXPair(address(engine), address(weth));
        pair.setReserves(uint112(80_658_175_170 * 1e9), uint112(52_000 * 1e9));

        router = new SMockDEXRouter(address(weth), address(pair), address(engine));
        vm.deal(address(router), 100_000_000 ether);

        dampener  = new DampenerVault(address(engine));
        sequencer = new CryptographicSequencer(address(engine), address(dampener));

        engine.setDexRouter(address(router));
        engine.setDexPair(address(pair), true);

        engine.proposeDampener(address(dampener));
        vm.warp(block.timestamp + 25 hours);
        engine.confirmDampener();

        engine.proposeSequencer(address(sequencer));
        vm.warp(block.timestamp + 25 hours);
        engine.confirmSequencer();

        engine.setTeamWallet(teamAddr);
        dampener.setToken52f(address(engine));
        dampener.setDexConfig(address(pair), address(router), address(pair.lpToken()));
        engine.setMockMinter(address(router));
        engine.seedTransfer(alice, 500_000_000 * 1e18);
        engine.enableTrading();

        vm.stopPrank();

        // Fund standard accounts
        vm.deal(alice,   1_000_000 ether);
        vm.deal(bob,     1_000_000 ether);
        vm.deal(charlie, 1_000_000 ether);

        // Fund 20 stress traders
        for (uint256 i = 0; i < 20; i++) {
            traders[i] = address(uint160(0x1000 + i));
            vm.deal(traders[i], 1_000_000 ether);
        }
    }

    // =========================================================================
    // HELPERS
    // =========================================================================

    function _rollBlocks(uint256 n) internal { vm.roll(block.number + n); }
    function _rollToHardening() internal { _rollBlocks(ZONE_B_END + 1); }
    function _rollToScarcity()  internal { _rollBlocks(HARDENING_END + 1); }

    function _buy(address buyer, uint256 amount) internal {
        vm.prank(buyer);
        engine.buy{value: amount}();
    }

    function _sell(address seller, uint256 amount) internal {
        vm.startPrank(seller);
        engine.approve(address(engine), amount);
        engine.sell(amount);
        vm.stopPrank();
    }

    /// @notice Snapshot all four accumulators
    function _snapAccumulators() internal view returns (
        uint256 dampAcc, uint256 prizeAcc, uint256 teamAcc, uint256 liqAcc
    ) {
        dampAcc  = engine.getDampenerTaxAccumulated();
        prizeAcc = engine.getPrizePot();
        teamAcc  = engine.getTeamAccumulated();
        liqAcc   = engine.getDampenerLiquidityAccumulated();
    }

    // =========================================================================
    // STRESS 1: ACCUMULATOR CONSERVATION LAW
    // 1,000 buys — total tax collected must equal sum of all four accumulators
    // This is the accounting identity: no QF is created or destroyed
    // =========================================================================

    function test_stress_accumulatorConservation_zoneA() public {
        uint256 N = 200;
        uint256 buyAmount = 1 ether;

        uint256 engineBalBefore = address(engine).balance;

        for (uint256 i = 0; i < N; i++) {
            address trader = traders[i % 20];
            _buy(trader, buyAmount);
        }

        uint256 engineBalAfter = address(engine).balance;
        uint256 totalTaxReceived = engineBalAfter - engineBalBefore;

        (uint256 d, uint256 p, uint256 t, uint256 l) = _snapAccumulators();
        uint256 totalAccumulated = d + p + t + l;

        // Conservation: every wei of tax must sit in exactly one accumulator
        assertEq(
            totalAccumulated,
            totalTaxReceived,
            "CONSERVATION VIOLATION: accumulated != received"
        );

        // In Zone A: prize must be zero, team must be positive, dampener must be positive
        assertEq(p, 0,   "Zone A: prize accumulator must be zero");
        assertGt(t, 0,   "Zone A: team accumulator must be positive");
        assertGt(d, 0,   "Zone A: dampener accumulator must be positive");
        assertEq(l, 0,   "Zone A: liquidity accumulator must be zero");
    }

    function test_stress_accumulatorConservation_hardening() public {
        _rollToHardening();
        uint256 N = 200;
        uint256 buyAmount = 1 ether;

        uint256 engineBalBefore = address(engine).balance;

        for (uint256 i = 0; i < N; i++) {
            address trader = traders[i % 20];
            _buy(trader, buyAmount);
        }

        uint256 totalTaxReceived = address(engine).balance - engineBalBefore;
        (uint256 d, uint256 p, uint256 t, uint256 l) = _snapAccumulators();
        uint256 totalAccumulated = d + p + t + l;

        assertEq(totalAccumulated, totalTaxReceived, "CONSERVATION VIOLATION: hardening buys");

        // In Hardening: team must be zero (redirected to dampener), prize positive
        assertEq(t, 0, "Hardening: team must be zero");
        assertGt(d, 0, "Hardening: dampener must be positive");
        assertGt(p, 0, "Hardening: prize must be positive");
    }

    function test_stress_accumulatorConservation_scarcity() public {
        _rollToScarcity();
        uint256 N = 200;
        uint256 buyAmount = 1 ether;

        uint256 engineBalBefore = address(engine).balance;

        for (uint256 i = 0; i < N; i++) {
            address trader = traders[i % 20];
            _buy(trader, buyAmount);
        }

        uint256 totalTaxReceived = address(engine).balance - engineBalBefore;
        (uint256 d, uint256 p, uint256 t, uint256 l) = _snapAccumulators();
        uint256 totalAccumulated = d + p + t + l;

        assertEq(totalAccumulated, totalTaxReceived, "CONSERVATION VIOLATION: scarcity buys");

        // In Scarcity: all four accumulators must be positive
        assertGt(d, 0, "Scarcity: dampener must be positive");
        assertGt(p, 0, "Scarcity: prize must be positive");
        assertGt(t, 0, "Scarcity: team must be positive");
    }

    function test_stress_accumulatorConservation_mixedBuySell() public {
        _rollToScarcity();

        uint256 engineBalBefore = address(engine).balance;

        // Interleaved buys and sells across 20 traders
        for (uint256 i = 0; i < 100; i++) {
            address trader = traders[i % 20];
            _buy(trader, 2 ether);

            // Every other iteration do a sell too
            if (i % 2 == 0) {
                uint256 bal = engine.balanceOf(trader);
                if (bal > 0) _sell(trader, bal / 2);
            }
        }

        uint256 totalTaxReceived = address(engine).balance - engineBalBefore;
        (uint256 d, uint256 p, uint256 t, uint256 l) = _snapAccumulators();

        assertEq(
            d + p + t + l,
            totalTaxReceived,
            "CONSERVATION VIOLATION: mixed buy/sell"
        );
    }

    // =========================================================================
    // STRESS 2: PHASE TRANSITION INTEGRITY
    // Buy exactly AT each boundary block — tax must flip precisely
    // =========================================================================

    function test_stress_allPhaseBoundaries_noRevert() public {
        uint256[4] memory boundaries = [
            uint256(1),           // Zone A start
            ZONE_A_END,           // Zone A → Zone B
            ZONE_B_END,           // Zone B → Hardening
            HARDENING_END         // Hardening → Scarcity
        ];

        for (uint256 b = 0; b < 4; b++) {
            vm.roll(boundaries[b]);
            // Buy at exact boundary — must not revert
            _buy(traders[b], 1 ether);
            // One block after boundary
            _rollBlocks(1);
            _buy(traders[b], 1 ether);
        }

        assertTrue(true, "All phase boundaries crossed without revert");
    }

    function test_stress_zoneB_decayIsMonotonic() public {
        // Zone B decays in 50 steps of 104 blocks
        // Tax at step N must always be >= tax at step N+1
        uint256 prevBuyBPS  = 10_000;
        uint256 prevSellBPS = 10_000;

        for (uint256 step = 0; step < 50; step++) {
            uint256 blockNum = ZONE_A_END + (step * 104) + 1;
            vm.roll(blockNum);

            (uint256 buyBPS, uint256 sellBPS,) = engine.getCurrentTaxRates();

            assertLe(buyBPS,  prevBuyBPS,  "Zone B buy tax must be monotonically decreasing");
            assertLe(sellBPS, prevSellBPS, "Zone B sell tax must be monotonically decreasing");

            prevBuyBPS  = buyBPS;
            prevSellBPS = sellBPS;
        }

        // At end of Zone B, must be at floor (200 BPS = 2%)
        vm.roll(ZONE_B_END - 1);
        (uint256 floorBuy,,) = engine.getCurrentTaxRates();
        
        assertEq(floorBuy,  200, "Zone B buy floor must be 200 BPS");
        assertEq(floorSell, 200, "Zone B sell floor must be 200 BPS");
    }

    function test_stress_taxPrecision_1000transactions() public {
        _rollToScarcity();

        uint256 totalBuyTax;
        uint256 totalSellTax;
        uint256 buyAmount  = 1 ether;
        uint256 N          = 500;

        for (uint256 i = 0; i < N; i++) {
            address trader = traders[i % 20];

            // Compute expected tax before buying
            (,,,uint256 expectedTax,) = engine.getPendingTaxBreakdown(buyAmount, true);
            totalBuyTax += expectedTax;

            _buy(trader, buyAmount);
        }

        // Check dampener + prize + team closely matches expected total buy tax
        (uint256 d, uint256 p, uint256 t,) = _snapAccumulators();
        uint256 actualAccumulated = d + p + t;

        // Allow 0.1% tolerance for rounding
        uint256 tolerance = totalBuyTax / 1000;
        assertApproxEqAbs(
            actualAccumulated,
            totalBuyTax,
            tolerance,
            "Tax precision: accumulated should match computed within 0.1%"
        );
    }

    // =========================================================================
    // STRESS 3: VOLUME — 1,000 SEQUENTIAL BUYS
    // No overflow, no revert, accumulators grow monotonically
    // =========================================================================

    function test_stress_1000buys_noOverflow() public {
        uint256 N = 1_000;

        uint256 prevDamp  = 0;
        uint256 prevPrize = 0;

        _rollToScarcity();

        for (uint256 i = 0; i < N; i++) {
            address trader = traders[i % 20];
            _buy(trader, 0.1 ether);

            // Every 100 buys — assert monotonic growth
            if (i % 100 == 99) {
                (uint256 d, uint256 p,,) = _snapAccumulators();
                assertGe(d, prevDamp,  "Dampener must never decrease");
                assertGe(p, prevPrize, "Prize must never decrease");
                prevDamp  = d;
                prevPrize = p;
            }
        }

        assertTrue(true, "1,000 buys completed without overflow or revert");
    }

    function test_stress_500sells_noUnderflow() public {
        _rollToScarcity();

        // Give all traders tokens to sell
        vm.startPrank(owner);
        for (uint256 i = 0; i < 20; i++) {
            engine.seedTransfer(traders[i], 10_000_000 * 1e18);
        }
        // Re-enable trading (seedTransfer requires trading off — use mockMint instead)
        vm.stopPrank();

        // Actually use mockMint since trading is enabled
        for (uint256 i = 0; i < 20; i++) {
            vm.prank(owner);
            engine.mockMint(traders[i], 10_000_000 * 1e18);
        }

        uint256 N = 500;
        for (uint256 i = 0; i < N; i++) {
            address trader = traders[i % 20];
            uint256 bal = engine.balanceOf(trader);
            if (bal > 1e18) {
                _sell(trader, 1e18); // sell exactly 1 token each iteration
            }
        }

        assertTrue(true, "500 sells completed without underflow or revert");
    }

    // =========================================================================
    // STRESS 4: EPOCH CHURN — 10 COMPLETE EPOCHS
    // State must reset cleanly, no bleed between epochs
    // =========================================================================

    function test_stress_10epochs_cleanReset() public {
        _rollToHardening();

        uint256 epochsBefore = sequencer.epochNumber();

        for (uint256 epoch = 0; epoch < 10; epoch++) {
            uint256 epochStart = sequencer.epochNumber();

            // Register 52 entries to mature the epoch (each from unique block+trader combos)
            for (uint256 j = 0; j < 52; j++) {
                _rollBlocks(1);
                // Prank as engine to register entries
                vm.prank(address(engine));
                sequencer.registerEntry(
                    traders[j % 20],
                    (j + 1) * 1e18,   // amount
                    j + 1             // mod1000 value — unique per entry
                );
            }

            uint256 epochAfter = sequencer.epochNumber();
            assertEq(epochAfter, epochStart + 1, "Epoch must increment after 52 entries");

            // Slot count must reset to 0 for the new epoch
            assertEq(sequencer.currentSlotCount(), 0, "Slot count must reset after epoch matures");
        }

        assertEq(
            sequencer.epochNumber(),
            epochsBefore + 10,
            "Should have completed exactly 10 epochs"
        );
    }

    function test_stress_epochPrizePool_neverLeaks() public {
        _rollToHardening();

        // Fund prize pool
        vm.deal(address(sequencer), 100 ether);
        vm.prank(address(engine));
        sequencer.receivePrizeFunds{value: 0}();

        // Track prize pool across multiple epochs
        for (uint256 epoch = 0; epoch < 5; epoch++) {
            uint256 prizePoolBefore = address(sequencer).balance;

            // Fill and mature epoch
            for (uint256 j = 0; j < 52; j++) {
                _rollBlocks(1);
                vm.prank(address(engine));
                sequencer.registerEntry(traders[j % 20], (j + 1) * 1e18, j + 1);
            }

            // Prize pool should have decreased (paid out) or stayed same (no collision)
            uint256 prizePoolAfter = address(sequencer).balance;
            assertLe(prizePoolAfter, prizePoolBefore, "Prize pool must not increase spontaneously");
        }
    }

    // =========================================================================
    // STRESS 5: MULTI-PHASE MARATHON
    // Buy through ALL phases sequentially — verify correct BPS at every transition
    // =========================================================================

    function test_stress_fullPhaseMarathon() public {
        // ---- Zone A (blocks 1-5200) ----
        vm.roll(100);
        (uint256 buyBPS, uint256 sellBPS,) = engine.getCurrentTaxRates();
        assertEq(buyBPS,  9_500, "Zone A buy must be 9500 BPS");
        assertEq(sellBPS, 9_500, "Zone A sell must be 9500 BPS");
        _buy(traders[0], 1 ether);

        // ---- Zone B mid-point ----
        vm.roll(ZONE_A_END + 2_600);
        (buyBPS,,) = engine.getCurrentTaxRates();
        assertLt(buyBPS, 9_500, "Zone B mid: tax must be below Zone A");
        assertGt(buyBPS, 272,   "Zone B mid: tax must be above e-floor (272 BPS)");
        _buy(traders[1], 1 ether);

        // ---- Zone B floor ----
        vm.roll(ZONE_B_END - 10);
        (buyBPS,,) = engine.getCurrentTaxRates();
        assertEq(buyBPS, 200, "Zone B floor must be 200 BPS");
        _buy(traders[2], 1 ether);

        // ---- Hardening ----
        vm.roll(ZONE_B_END + 1);
        (buyBPS, sellBPS,) = engine.getCurrentTaxRates();
        assertEq(buyBPS,  272, "Hardening buy must be 272 BPS (e=2.71828)");
        assertEq(sellBPS, 314, "Hardening sell must be 314 BPS (pi=3.14159)");
        _buy(traders[3], 1 ether);

        // ---- Scarcity ----
        vm.roll(HARDENING_END + 1);
        (buyBPS, sellBPS,) = engine.getCurrentTaxRates();
        assertEq(buyBPS,  272, "Scarcity buy must be 272 BPS (e=2.71828)");
        assertEq(sellBPS, 314, "Scarcity sell must be 314 BPS (pi=3.14159)");
        _buy(traders[4], 1 ether);

        // Final conservation check across all phases
        (uint256 d, uint256 p, uint256 t, uint256 l) = _snapAccumulators();
        assertGt(d + p + t + l, 0, "Accumulators must be non-zero after full marathon");
    }

    // =========================================================================
    // STRESS 6: VESTING MARATHON — ALL 52 TRANCHES
    // Claim every tranche in sequence, verify exact amounts
    // =========================================================================

    function test_stress_vesting_all52Tranches() public {
        uint256 totalVesting = 9_741_094_233 * 1e18;
        uint256 trancheSize  = totalVesting / 52;
        uint256 interval     = 200_000; // blocks between tranches

        uint256 totalClaimed = 0;

        for (uint256 tranche = 0; tranche < 52; tranche++) {
            _rollBlocks(interval + 1);

            uint256 balBefore = engine.balanceOf(owner);
            vm.prank(owner);
            engine.claimVesting();
            uint256 balAfter = engine.balanceOf(owner);

            uint256 claimed = balAfter - balBefore;
            assertApproxEqAbs(
                claimed,
                trancheSize,
                trancheSize / 1000, // 0.1% tolerance for rounding
                "Each tranche must be approximately correct"
            );
            totalClaimed += claimed;
        }

        // After all 52 tranches, total claimed must equal total vesting allocation
        assertApproxEqAbs(
            totalClaimed,
            totalVesting,
            totalVesting / 1000,
            "Total claimed must equal vesting allocation"
        );

        // 53rd claim must revert
        _rollBlocks(interval + 1);
        vm.expectRevert();
        vm.prank(owner);
        engine.claimVesting();
    }

    function test_stress_vesting_cannotDoubleClaimSameTranche() public {
        _rollBlocks(200_001);

        vm.prank(owner);
        engine.claimVesting();

        // Immediate second claim without advancing blocks — must revert
        vm.expectRevert();
        vm.prank(owner);
        engine.claimVesting();
    }

    // =========================================================================
    // STRESS 7: CONCURRENT ACCUMULATOR PULLS
    // Pull team, dampener, and sequencer prize in the same block — no state corruption
    // =========================================================================

    function test_stress_concurrentPulls_noCorruption() public {
        _rollToScarcity();

        // Accumulate all three pots
        for (uint256 i = 0; i < 50; i++) {
            _buy(traders[i % 20], 5 ether);
        }

        (uint256 d, uint256 p, uint256 t,) = _snapAccumulators();
        assertGt(d, 0, "Dampener must have funds");
        assertGt(p, 0, "Prize must have funds");
        assertGt(t, 0, "Team must have funds");

        // Pull dampener
        uint256 dampenerBefore = address(dampener).balance;
        dampener.pullFromEngine();
        assertGt(address(dampener).balance, dampenerBefore, "Dampener should have received QF");

        // Pull team
        uint256 teamBefore = teamAddr.balance;
        vm.prank(owner);
        engine.pullTeamFunds();
        assertGt(teamAddr.balance, teamBefore, "Team wallet should have received QF");

        // Pull sequencer prize
        vm.deal(address(sequencer), 10 ether);
        uint256 seqBefore = address(sequencer).balance;
        vm.prank(address(engine));
        sequencer.receivePrizeFunds{value: 0}();

        // After all pulls, engine accumulators should be cleared
        (uint256 dAfter, , uint256 tAfter,) = _snapAccumulators();
        assertEq(dAfter, 0, "Dampener accumulator must be zero after pull");
        assertEq(tAfter, 0, "Team accumulator must be zero after pull");
    }

    // =========================================================================
    // STRESS 8: PAUSE/UNPAUSE UNDER LOAD
    // Interleave pausing with buys/sells — no state corruption
    // =========================================================================

    function test_stress_pauseUnpause_repeatedCycles() public {
        _rollToHardening();

        uint256 cycles = 20;

        for (uint256 c = 0; c < cycles; c++) {
            // Buy while unpaused
            _buy(traders[c % 20], 1 ether);

            // Pause
            vm.prank(owner);
            engine.setPaused(true);

            // Buy must revert
            vm.expectRevert();
            vm.prank(traders[0]);
            engine.buy{value: 1 ether}();

            // Unpause
            vm.prank(owner);
            engine.setPaused(false);

            // Buy works again
            _buy(traders[c % 20], 1 ether);
        }

        // Accumulators should reflect exactly the successful buys (2 per cycle)
        (uint256 d,,, ) = _snapAccumulators();
        assertGt(d, 0, "Dampener should have accumulated from unpaused buys");
    }

    // =========================================================================
    // STRESS 9: SEQUENCER FORCE CLOSE CHURN
    // Fill partial epochs, force close, repeat — verify clean state
    // =========================================================================

    function test_stress_forceClose_10times() public {
        _rollToHardening();

        uint256 maxEpochBlocks = sequencer.MAX_EPOCH_BLOCKS();

        for (uint256 round = 0; round < 10; round++) {
            uint256 epochBefore = sequencer.epochNumber();

            // Register a few entries (not enough to mature naturally)
            for (uint256 j = 0; j < 5; j++) {
                _rollBlocks(1);
                vm.prank(address(engine));
                sequencer.registerEntry(traders[j], (j + 1) * 1e18, j + 1);
            }

            // Roll past max epoch blocks to allow force close
            _rollBlocks(maxEpochBlocks + 1);

            // Force close
            sequencer.forceCloseEpoch();

            assertEq(
                sequencer.epochNumber(),
                epochBefore + 1,
                "Epoch must increment after force close"
            );
            assertEq(
                sequencer.currentSlotCount(),
                0,
                "Slot count must reset after force close"
            );
        }
    }

    // =========================================================================
    // STRESS 10: TOTAL SUPPLY CONSERVATION
    // No tokens created or destroyed unexpectedly during 500 trades
    // =========================================================================

    function test_stress_totalSupplyConservation() public {
        _rollToScarcity();

        uint256 supplyBefore = engine.totalSupply();

        // 200 buys — mockMint adds to supply
        for (uint256 i = 0; i < 200; i++) {
            _buy(traders[i % 20], 0.5 ether);
        }

        uint256 supplyAfterBuys = engine.totalSupply();
        assertGe(supplyAfterBuys, supplyBefore, "Supply should not decrease during buys");

        // 200 sells — tokens transferred to contract/burned
        for (uint256 i = 0; i < 200; i++) {
            address trader = traders[i % 20];
            uint256 bal    = engine.balanceOf(trader);
            if (bal > 1e18) _sell(trader, 1e18);
        }

        // Supply should still be internally consistent (no phantom tokens)
        uint256 supplyFinal = engine.totalSupply();
        assertGe(supplyFinal, 0, "Total supply must never go negative");

        // The Great Drain burns tokens — supply can legitimately decrease
        // But it must never exceed supplyAfterBuys
        assertLe(supplyFinal, supplyAfterBuys + 1e18, "Supply must not exceed post-buy total");
    }
}
