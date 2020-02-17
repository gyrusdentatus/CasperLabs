package io.casperlabs.casper

import io.casperlabs.casper.Estimator.BlockHash
import io.casperlabs.casper.consensus.info.BlockInfo
import io.casperlabs.casper.consensus.Deploy
import io.casperlabs.casper.consensus.Block.ProcessedDeploy
import simulacrum.typeclass

/** Emit events based on the local deploy buffer, which are only
  * observed on the nodes where the deploy was sent.
  */
@typeclass trait DeployEventEmitter[F[_]] {
  def deployAdded(deploy: Deploy): F[Unit]
  def deployDiscarded(deployHash: DeployHash, message: String): F[Unit]
  def deployRequeued(deployHash: DeployHash): F[Unit]
}

/** Emit events based on whole blocks. Includes raising events for the deploys embedded in the blocks. */
@typeclass trait BlockEventEmitter[F[_]] {
  def blockAdded(blockInfo: BlockInfo): F[Unit]
  def newLastFinalizedBlock(
      lfb: BlockHash,
      indirectlyFinalized: Set[BlockHash]
  ): F[Unit]
}

/** Emit events to be observed by the outside world. */
@typeclass trait EventEmitter[F[_]] extends BlockEventEmitter[F] with DeployEventEmitter[F]
