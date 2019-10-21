#!/usr/bin/env bash
parallel-ssh -O "StrictHostKeyChecking no" -h ip_list.txt -l ubuntu -p 1000 pkill -f conflux
parallel-scp -O "StrictHostKeyChecking no" -h ip_list.txt -l ubuntu -p 1000 ../../target/release/conflux ~
