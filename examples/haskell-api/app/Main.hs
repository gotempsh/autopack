{-# LANGUAGE OverloadedStrings #-}
-- A tiny HTTP responder built on sockets, so the example exercises the build
-- rather than a web framework's dependency tree.
module Main (main) where

import Control.Exception (bracket)
import Control.Monad (forever, void)
import qualified Data.ByteString.Char8 as BS
import Data.Maybe (fromMaybe)
import Network.Socket
import Network.Socket.ByteString (recv, sendAll)
import System.Environment (lookupEnv)
import System.IO (BufferMode (LineBuffering), hSetBuffering, stdout)

main :: IO ()
main = do
    hSetBuffering stdout LineBuffering
    port <- fromMaybe "3000" <$> lookupEnv "PORT"
    addr <- resolve port
    bracket (open addr) close (serve port)

resolve :: String -> IO AddrInfo
resolve port = do
    let hints = defaultHints{addrFlags = [AI_PASSIVE], addrSocketType = Stream}
    addrs <- getAddrInfo (Just hints) Nothing (Just port)
    case addrs of
        (addr : _) -> pure addr
        [] -> ioError (userError ("no address for port " <> port))

open :: AddrInfo -> IO Socket
open addr = do
    sock <- socket (addrFamily addr) (addrSocketType addr) (addrProtocol addr)
    setSocketOption sock ReuseAddr 1
    bind sock (addrAddress addr)
    listen sock 16
    pure sock

serve :: String -> Socket -> IO ()
serve port sock = do
    putStrLn ("listening on " <> port)
    forever $ do
        (conn, _) <- accept sock
        void (recv conn 4096)
        sendAll conn response
        close conn

response :: BS.ByteString
response =
    BS.concat
        [ "HTTP/1.1 200 OK\r\n"
        , "Content-Type: text/plain\r\n"
        , "Content-Length: "
        , BS.pack (show (BS.length body))
        , "\r\n"
        , "Connection: close\r\n\r\n"
        , body
        ]

body :: BS.ByteString
body = "hello from autopack\n"
