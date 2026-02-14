"""
Embedding Service for AEGIS Cortex

Generates semantic embeddings using sentence-transformers for pattern storage.
"""

import grpc
from concurrent import futures
import logging
from sentence_transformers import SentenceTransformer
import numpy as np

# Import generated protobuf code
import embedding_pb2
import embedding_pb2_grpc

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

class EmbeddingServicer(embedding_pb2_grpc.EmbeddingServiceServicer):
    """Implementation of EmbeddingService"""
    
    def __init__(self, model_name='all-MiniLM-L6-v2'):
        """
        Initialize the embedding service
        
        Args:
            model_name: Sentence transformer model to use
                       Default: all-MiniLM-L6-v2 (384-dim, fast, good quality)
        """
        logger.info(f"Loading model: {model_name}")
        self.model = SentenceTransformer(model_name)
        self.model_name = model_name
        self.dimension = self.model.get_sentence_embedding_dimension()
        logger.info(f"Model loaded. Dimension: {self.dimension}")
    
    def GenerateEmbedding(self, request, context):
        """Generate embedding for a single text"""
        try:
            logger.debug(f"Generating embedding for text: {request.text[:100]}...")
            
            # Generate embedding
            embedding = self.model.encode(request.text, convert_to_numpy=True)
            
            return embedding_pb2.EmbeddingResponse(
                embedding=embedding.tolist(),
                dimension=self.dimension,
                model=self.model_name
            )
        except Exception as e:
            logger.error(f"Error generating embedding: {e}")
            context.set_code(grpc.StatusCode.INTERNAL)
            context.set_details(str(e))
            return embedding_pb2.EmbeddingResponse()
    
    def GenerateBatch(self, request, context):
        """Generate embeddings for multiple texts"""
        try:
            logger.debug(f"Generating batch embeddings for {len(request.texts)} texts")
            
            # Generate embeddings in batch (more efficient)
            embeddings = self.model.encode(request.texts, convert_to_numpy=True)
            
            responses = []
            for embedding in embeddings:
                responses.append(embedding_pb2.EmbeddingResponse(
                    embedding=embedding.tolist(),
                    dimension=self.dimension,
                    model=self.model_name
                ))
            
            return embedding_pb2.BatchEmbeddingResponse(embeddings=responses)
        except Exception as e:
            logger.error(f"Error generating batch embeddings: {e}")
            context.set_code(grpc.StatusCode.INTERNAL)
            context.set_details(str(e))
            return embedding_pb2.BatchEmbeddingResponse()
    
    def HealthCheck(self, request, context):
        """Health check endpoint"""
        return embedding_pb2.HealthCheckResponse(
            healthy=True,
            model=self.model_name,
            dimension=self.dimension
        )

def serve(port=50052):
    """Start the gRPC server"""
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
    embedding_pb2_grpc.add_EmbeddingServiceServicer_to_server(
        EmbeddingServicer(), server
    )
    server.add_insecure_port(f'[::]:{port}')
    server.start()
    logger.info(f"Embedding service started on port {port}")
    server.wait_for_termination()

if __name__ == '__main__':
    serve()
